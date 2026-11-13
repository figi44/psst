//! `SIGHUP` reloads `profiles.toml` and `dictionary.toml` (plan 6.5), keeping a file's old
//! version when the new one fails validation.

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

use crate::harness::*;

const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);

fn hangup(server: &TestServer) {
    kill(Pid::from_raw(server.pid() as i32), Signal::SIGHUP).expect("send SIGHUP");
}

/// Dictates once with `profile`; the engine request's language and bias terms, or the error.
async fn dictate(server: &TestServer, profile: &str) -> Result<(String, serde_json::Value), ErrorCode> {
    let mut client = Client::connect(server).await;
    let id = SessionId::new_v4();
    client
        .start_with(StartMsg {
            session_id: id,
            profile: Some(profile.into()),
            audio: AudioFormat::default(),
            mode: Mode::Upload,
            resume: false,
            eval: None,
        })
        .await;
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
    assert_eq!(dictate(&server, "czech").await, Ok(("cs".into(), json!(["Hyprland"]))));

    fs::write(config.join("profiles.toml"), "[profiles.default]\n[profiles.german]\nlanguage = \"de\"\n").unwrap();
    fs::write(config.join("dictionary.toml"), "[[terms]]\ntext = \"Wayland\"\n").unwrap();
    hangup(&server);
    server.wait_for_log("profiles.toml: reloaded", 1, RELOAD_TIMEOUT);
    server.wait_for_log("dictionary.toml: reloaded", 1, RELOAD_TIMEOUT);
    assert_eq!(dictate(&server, "german").await, Ok(("de".into(), json!(["Wayland"]))));
    assert_eq!(dictate(&server, "czech").await, Err(ErrorCode::BadRequest));
    assert_eq!(requests_of(&server, "load").len(), 1, "a reload doesn't restart the engine");
}
