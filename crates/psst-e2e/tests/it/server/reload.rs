use std::{fs, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use psst_config::{ConfigFile, DictionaryFile, ProfilesFile};
use psst_engine_proto::fake::Script;
use psst_proto::{
    ErrorCode, SessionId,
    ws::{AudioFormat, Mode, StartMsg},
};
use psst_testkit::TestServer;
use serde_json::json;

use super::harness::*;

const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);

fn hangup(server: &TestServer) {
    kill(Pid::from_raw(server.pid() as i32), Signal::SIGHUP).expect("send SIGHUP");
}

fn start(session_id: SessionId, profile: &str, resume: bool) -> StartMsg {
    StartMsg {
        session_id,
        profile: Some(profile.into()),
        audio: AudioFormat::default(),
        mode: Mode::Upload,
        resume,
        eval: None,
    }
}

async fn dictate(server: &TestServer, profile: &str, id: SessionId) -> Result<(String, serde_json::Value), ErrorCode> {
    let mut client = Client::connect(server).await;
    client.start_with(start(id, profile, false)).await;
    match client.recv().await {
        psst_proto::ws::ServerMsg::Ready(_) => {}
        psst_proto::ws::ServerMsg::Error(err) => return Err(err.code),
        other => panic!("unexpected {other:?}"),
    }
    client.audio(SECOND / 4).await;
    client.stop(id, SECOND as u64 / 4).await;
    client.final_msg().await;
    let request = transcribes(server).pop().expect("a transcribe request");
    Ok((request.header["language"].as_str().unwrap().to_owned(), request.header["bias"]["terms"].clone()))
}

#[tokio::test]
async fn sighup_reloads_profiles_and_dictionary_and_keeps_versions_that_fail_validation() {
    let server = server(Script::default(), |cfg| {
        cfg.profiles =
            ProfilesFile::from_toml_str("[profiles.default]\nlanguage = \"en\"\n[profiles.czech]\nlanguage = \"cs\"\n")
                .unwrap();
        cfg.dictionary = DictionaryFile::from_toml_str("[[terms]]\ntext = \"Hyprland\"\n").unwrap();
    });
    let config = server.config_dir();

    fs::write(config.join("profiles.toml"), "[profiles.czech]\nlanguage = \"cs\"\n").unwrap();
    fs::write(config.join("dictionary.toml"), "[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"gpu\"\n").unwrap();
    hangup(&server);
    server.wait_for_log("keeping the previous profiles.toml", 1, RELOAD_TIMEOUT);
    server.wait_for_log("keeping the previous dictionary.toml", 1, RELOAD_TIMEOUT);
    let czech_session = SessionId::new_v4();
    assert_eq!(dictate(&server, "czech", czech_session).await, Ok(("cs".into(), json!(["Hyprland"]))));

    fs::write(config.join("profiles.toml"), "[profiles.default]\n[profiles.german]\nlanguage = \"de\"\n").unwrap();
    fs::write(config.join("dictionary.toml"), "[[terms]]\ntext = \"Wayland\"\n").unwrap();
    hangup(&server);
    server.wait_for_log("profiles.toml: reloaded", 1, RELOAD_TIMEOUT);
    server.wait_for_log("dictionary.toml: reloaded", 1, RELOAD_TIMEOUT);
    assert_eq!(dictate(&server, "german", SessionId::new_v4()).await, Ok(("de".into(), json!(["Wayland"]))));
    assert_eq!(dictate(&server, "czech", SessionId::new_v4()).await, Err(ErrorCode::BadRequest));

    // A finished session resumes even though its profile is gone now.
    let mut client = Client::connect(&server).await;
    client.start_with(start(czech_session, "czech", true)).await;
    let ready = client.ready().await;
    assert!(ready.resumed && !ready.need_audio, "{ready:?}");
    assert_eq!(client.final_msg().await.session_id, czech_session);
    assert_eq!(requests_of(&server, "load").len(), 1, "a reload doesn't restart the engine");
}
