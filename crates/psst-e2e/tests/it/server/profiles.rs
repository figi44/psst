//! Profiles stored in the server's database over `profiles.toml`: whole
//! profiles by name through `/api/profiles`, checked like `check-config`, reaching new sessions
//! without an engine restart, and exported with `psst-server profiles export` to fold back into
//! the file.

use std::{fs, process::Command, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_config::{ConfigFile, ProfilesFile};
use psst_proto::{
    SessionId,
    api::{ApiErrorCode, ProfileChange, ProfileLayer, ProfilesView, routes},
    ws::{AudioFormat, Mode, StartMsg},
};
use psst_testkit::TestServer;
use serde_json::{Value, json};

use super::harness::*;

const FILE: &str = r#"
[profiles.default]
language = "en"
prompt_style = "I'm updating the NixOS config."

[profiles.czech]
language = "cs"
prompt_style = "Upravuji konfiguraci."

[fillers]
en = ["um"]
cs = ["ehm"]
"#;

fn profiles_server() -> TestServer {
    server(script(vec![]), |cfg| cfg.profiles = ProfilesFile::from_toml_str(FILE).unwrap())
}

fn api(server: &TestServer) -> ApiClient {
    ApiClient::new(server.endpoint(), None)
}

async fn view(api: &ApiClient) -> ProfilesView {
    api.call(Method::Get, routes::PROFILES, None::<&()>).await.expect("GET /api/profiles")
}

/// Every profile with its layer and language.
async fn layers(api: &ApiClient) -> Vec<(String, ProfileLayer, String)> {
    view(api).await.profiles.into_iter().map(|p| (p.name, p.layer, p.profile.language)).collect()
}

async fn put(api: &ApiClient, name: &str, profile: Value) -> Result<ProfileChange, ApiCallError> {
    api.call(Method::Put, &routes::with_name(routes::PROFILE, name), Some(&profile)).await
}

async fn reset(api: &ApiClient, name: &str) -> Result<ProfileChange, ApiCallError> {
    api.call(Method::Post, &routes::with_name(routes::PROFILE_RESET, name), None::<&()>).await
}

#[track_caller]
fn refused<T: std::fmt::Debug>(result: Result<T, ApiCallError>) -> (u16, ApiErrorCode, String) {
    match result {
        Err(ApiCallError::Api { status, error }) => (status, error.code, error.message),
        other => panic!("expected an API error, got {other:?}"),
    }
}

/// Dictates "fake transcript" with `profile`; the final text and the engine request's language
/// and prompt style.
async fn dictate(server: &TestServer, profile: &str) -> (String, String, String) {
    let mut client = Client::connect(server).await;
    let session_id = SessionId::new_v4();
    client
        .start_with(StartMsg {
            session_id,
            profile: Some(profile.into()),
            audio: AudioFormat::default(),
            mode: Mode::Upload,
            resume: false,
            eval: None,
        })
        .await;
    client.ready().await;
    client.audio(SECOND / 4).await;
    client.stop(session_id, SECOND as u64 / 4).await;
    let text = client.final_msg().await.r#final.text;
    let request = transcribes(server).pop().expect("a transcribe request");
    let field = |value: &Value| value.as_str().expect("a string").to_owned();
    (text, field(&request.header["language"]), field(&request.header["bias"]["style"]))
}

fn hangup(server: &TestServer) {
    kill(Pid::from_raw(server.pid() as i32), Signal::SIGHUP).expect("send SIGHUP");
}

#[tokio::test]
async fn stored_profiles_reach_new_sessions_and_fold_back_into_the_file() {
    let server = profiles_server();
    let api = api(&server);
    assert_eq!(
        dictate(&server, "czech").await,
        ("Fake transcript ".into(), "cs".into(), "Upravuji konfiguraci.".into())
    );

    let czech = json!({"language": "cs", "prompt_style": "Nový styl.", "rules": ["whitespace"]});
    let changed = put(&api, "czech", czech).await.unwrap().profile.expect("the stored profile");
    assert_eq!(changed.layer, ProfileLayer::Replaced);
    assert_eq!(changed.file.expect("the file's version").prompt_style, "Upravuji konfiguraci.");
    let notes = json!({"language": "en", "prompt_style": "Notes, in short sentences.", "rules": []});
    assert_eq!(put(&api, "notes", notes).await.unwrap().profile.unwrap().layer, ProfileLayer::Added);
    assert_eq!(
        layers(&api).await,
        [
            ("czech".into(), ProfileLayer::Replaced, "cs".into()),
            ("default".into(), ProfileLayer::File, "en".into()),
            ("notes".into(), ProfileLayer::Added, "en".into()),
        ]
    );

    // The next sessions use them, with their own rules: no capital or trailing space.
    assert_eq!(dictate(&server, "czech").await, ("fake transcript".into(), "cs".into(), "Nový styl.".into()));
    assert_eq!(dictate(&server, "notes").await.2, "Notes, in short sentences.");

    // The export is the file as sessions see it, which the running server reloads.
    let out = Command::new(SERVER)
        .args(["profiles", "export", "--socket"])
        .arg(server.socket())
        .output()
        .expect("run psst-server");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let export = String::from_utf8(out.stdout).unwrap();
    assert!(
        export.contains("# From the server's database, in place of profiles.toml's.\n[profiles.czech]"),
        "{export}"
    );
    assert!(
        export.contains("# From the server's database; profiles.toml doesn't have it.\n[profiles.notes]"),
        "{export}"
    );
    let exported = ProfilesFile::from_toml_str(&export).unwrap_or_else(|err| panic!("{err}\n{export}"));
    let file = ProfilesFile::from_toml_str(FILE).unwrap();
    assert_eq!((&exported.profiles["default"], &exported.fillers), (&file.profiles["default"], &file.fillers));
    assert_eq!(exported.profiles["czech"].prompt_style, "Nový styl.");
    assert!(exported.profiles["notes"].rules.is_empty());

    fs::write(server.config_dir().join("profiles.toml"), &export).unwrap();
    hangup(&server);
    server.wait_for_log("profiles.toml: reloaded", 1, Duration::from_secs(10));
    for name in ["czech", "notes"] {
        assert_eq!(reset(&api, name).await.unwrap().profile.unwrap().layer, ProfileLayer::File);
    }
    assert!(view(&api).await.profiles.iter().all(|p| p.layer == ProfileLayer::File));
    assert_eq!(dictate(&server, "czech").await, ("fake transcript".into(), "cs".into(), "Nový styl.".into()));
    assert_eq!(requests_of(&server, "load").len(), 1, "no change restarted the engine");
}

#[tokio::test]
async fn profile_changes_are_checked_like_check_config_and_kept_across_restarts() {
    let mut server = profiles_server();
    let api = api(&server);
    let before = view(&api).await;

    let refusals = [
        (json!({"rules": ["fillers", "shout"]}), "unknown rule `shout`"),
        (json!({"rules": ["fillers", "fillers"]}), "lists `fillers` twice"),
        (json!({"language": ""}), "profiles.czech.language must not be empty"),
        (json!({"command": ["sh", "-c", "true"]}), "unknown field `command`"),
    ];
    for (body, reason) in refusals {
        let (status, code, message) = refused(put(&api, "czech", body.clone()).await);
        assert_eq!((status, code), (400, ApiErrorCode::BadRequest), "{body}");
        assert!(message.contains(reason), "{body}: {message}");
    }
    let (status, _, message) = refused(reset(&api, "default").await);
    assert_eq!(status, 404, "{message}");
    assert_eq!(view(&api).await, before, "nothing refused changed anything");

    // Missing keys take the file's defaults; a long style is a warning, not an error.
    let long = "word ".repeat(100);
    let change = put(&api, "export", json!({"prompt_style": long})).await.unwrap();
    let profile = &change.profile.as_ref().expect("the added profile").profile;
    assert_eq!((profile.language.as_str(), profile.rules.len()), ("en", 4));
    assert_eq!(change.warnings.len(), 1);
    assert!(change.warnings[0].contains("profiles.export.prompt_style is over about 100 tokens"), "{change:?}");
    assert_eq!(view(&api).await.warnings, change.warnings);

    server.restart();
    let api = self::api(&server);
    assert!(layers(&api).await.contains(&("export".into(), ProfileLayer::Added, "en".into())));
    assert_eq!(reset(&api, "export").await.unwrap(), ProfileChange { profile: None, warnings: vec![] });
    assert_eq!(view(&api).await, before);
}
