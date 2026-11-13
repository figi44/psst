use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use psst_engine_proto::fake;
use psst_proto::api::{ModelState, ModelStatus};
use psst_server::{ServeConfig, serve_in_process};

const BIN: &str = env!("CARGO_BIN_EXE_psst-server");

fn model_status(socket: &Path) -> ModelStatus {
    let out = Command::new(BIN).args(["model", "status", "--json", "--socket"]).arg(socket).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn loads(script: &Path) -> usize {
    fake::read_request_log(script).unwrap().iter().filter(|r| r.header["type"] == "load").count()
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_in_process_server_restarts_its_engine_after_resume_and_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    let script = dir.path().join("engine.json");
    fs::write(&script, "{}").unwrap();
    fs::write(
        config_dir.join("server.toml"),
        format!("listen = []\n[model]\ncommand = [{BIN:?}, \"__fake-engine\", {:?}]\n", script.display().to_string()),
    )
    .unwrap();
    let socket = dir.path().join("client.sock");

    let server = serve_in_process(ServeConfig {
        config_dir,
        state_dir: dir.path().join("state"),
        cache_dir: dir.path().join("cache"),
        socket: Some(socket.clone()),
    })
    .await
    .unwrap();
    assert_eq!(server.socket(), socket);
    assert!(server.tcp_addrs().is_empty());
    wait_until("the model is loaded", || model_status(&socket).state == ModelState::Loaded).await;

    server.simulate_resume();
    wait_until("the engine is restarted", || loads(&script) == 2).await;
    wait_until("the model is loaded again", || model_status(&socket).state == ModelState::Loaded).await;

    server.shutdown().await.unwrap();
    assert!(!socket.exists(), "the socket file is removed");
    let requests = fake::read_request_log(&script).unwrap();
    assert_eq!(requests.last().unwrap().header["type"], "shutdown", "the engine is shut down");
}
