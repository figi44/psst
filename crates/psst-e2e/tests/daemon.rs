//! The `psst` daemon and CLI as built, end to end: `psst-e2e-daemon` runs in a headless sandbox
//! with audio replayed from a WAV and the `[insert] record` seam in place of Wayland.
//!
//! **The server is the scripted fake for now** (`psst_testkit::fake_server`), because
//! `psst-server serve` isn't implemented on main yet. [`Server`] is the one place to switch:
//! once it is, `Server::start` becomes `psst_testkit::spawn_server(SERVER, …)` with the fake
//! engine scripted to return the same texts, and `set_listening` becomes killing and restarting
//! that server on the same socket.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Output,
    thread,
    time::{Duration, Instant},
};

use psst_proto::Endpoint;
use psst_testkit::{
    Sandbox, TestClientConfig, TestDaemon,
    fake_server::{FakeServer, FakeServerConfig, Outcome},
    spawn_daemon, test_audio, write_wav,
};
use serde_json::Value;

const DAEMON: &str = env!("CARGO_BIN_EXE_psst-e2e-daemon");
const WAIT: Duration = Duration::from_secs(15);

/// The server under test; see the module docs.
struct Server(FakeServer);

impl Server {
    /// A server whose next dictations come back as `texts`, in order.
    fn start(texts: &[&str], tcp: bool) -> Server {
        let server = FakeServer::start(FakeServerConfig { tcp, ..Default::default() });
        for text in texts {
            server.push_outcome(Outcome::Final(text.to_string()));
        }
        Server(server)
    }

    fn endpoint(&self) -> Endpoint {
        self.0.endpoint()
    }

    fn set_listening(&self, listening: bool) {
        self.0.set_listening(listening);
    }
}

struct Client {
    sandbox: Sandbox,
    daemon: Option<TestDaemon>,
    cfg: TestClientConfig,
    inserted: PathBuf,
}

impl Client {
    fn start(server: Endpoint) -> Client {
        let sandbox = Sandbox::headless();
        let wav = sandbox.root.path().join("clip.wav");
        write_wav(&wav, &test_audio(3_000));
        let inserted = sandbox.root.path().join("inserted.jsonl");
        let mut cfg = TestClientConfig::new(server, wav);
        cfg.client.insert.record = Some(inserted.clone());
        let daemon = spawn_daemon(Path::new(DAEMON), &sandbox, cfg.clone());
        Client { sandbox, daemon: Some(daemon), cfg, inserted }
    }

    fn daemon(&self) -> &TestDaemon {
        self.daemon.as_ref().expect("running")
    }

    fn psst(&self, args: &[&str]) -> Output {
        self.daemon().run(args)
    }

    #[track_caller]
    fn ok(&self, args: &[&str]) -> String {
        let out = self.psst(args);
        assert!(
            out.status.success(),
            "psst {args:?} failed: {}{}\ndaemon log:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            self.daemon().log()
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn status(&self) -> Value {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    /// The commands the daemon sent its stand-in Wayland thread.
    fn commands(&self) -> Vec<Value> {
        fs::read_to_string(&self.inserted)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn inserted_texts(&self, cmd: &str) -> Vec<String> {
        self.commands().iter().filter(|c| c["cmd"] == cmd).map(|c| c["text"].as_str().unwrap().to_owned()).collect()
    }

    #[track_caller]
    fn wait_for(&self, what: &str, mut check: impl FnMut(&Client) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !check(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; daemon log:\n{}", self.daemon().log());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_idle(&self) {
        self.wait_for("idle", |c| c.status()["state"] == "idle");
    }

    fn restart(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            daemon.kill();
        }
        self.daemon = Some(spawn_daemon(Path::new(DAEMON), &self.sandbox, self.cfg.clone()));
    }
}

#[test]
fn toggle_twice_pastes_the_transcript_and_cleans_up() {
    let server = Server::start(&["Push the fix to psst. "], false);
    let client = Client::start(server.endpoint());
    assert!(client.ok(&["toggle"]).starts_with("recording"));
    thread::sleep(Duration::from_millis(500));
    assert!(client.ok(&["toggle"]).starts_with("stopped"));
    client.wait_idle();

    assert_eq!(client.inserted_texts("paste"), vec!["Push the fix to psst. "]);
    let paste = client.commands().into_iter().find(|c| c["cmd"] == "paste").unwrap();
    assert_eq!(paste["chord"], "ctrl+v", "no niri in the sandbox: the fallback chord");
    let state = client.daemon().state_dir();
    assert_eq!(fs::read_dir(state.join("recordings")).unwrap().count(), 0, "delivered recordings are deleted");
    let last: Value = serde_json::from_slice(&fs::read(state.join("last.json")).unwrap()).unwrap();
    assert_eq!(last["text"], "Push the fix to psst. ");
    assert_eq!(fs::metadata(state.join("last.json")).unwrap().permissions().mode() & 0o777, 0o600);
    let status = client.status();
    assert!(status["last_timings"]["stop_to_dispatch_ms"].is_u64(), "{status}");
}

#[test]
fn a_recording_made_offline_is_uploaded_once_the_server_is_back() {
    let server = Server::start(&["Recorded offline. "], false);
    let client = Client::start(server.endpoint());
    server.set_listening(false);
    client.ok(&["toggle"]);
    client.wait_for("the offline overlay", |c| {
        c.commands().iter().any(|cmd| cmd["cmd"] == "overlay" && cmd["state"]["kind"] == "offline")
    });
    thread::sleep(Duration::from_millis(300));
    server.set_listening(true);
    client.ok(&["toggle"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("paste"), vec!["Recorded offline. "]);
}

#[test]
fn a_recording_interrupted_by_a_crash_survives_for_retry() {
    let server = Server::start(&["Rescued. "], false);
    let mut client = Client::start(server.endpoint());
    server.set_listening(false);
    client.ok(&["toggle"]);
    thread::sleep(Duration::from_millis(400));
    client.restart(); // SIGKILL, then a fresh daemon

    let status = client.status();
    let retained = status["retained"].as_array().unwrap();
    assert_eq!(retained.len(), 1, "{status}");
    assert_eq!(retained[0]["state"], "incomplete");
    assert!(retained[0]["duration_ms"].as_u64().unwrap() >= 200);
    let text = client.ok(&["status"]);
    assert!(text.contains("psst retry --session"), "{text}");

    server.set_listening(true);
    client.ok(&["retry"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("copy"), vec!["Rescued. "], "an incomplete recording is copied, not pasted");
    assert!(client.inserted_texts("paste").is_empty());
}

#[test]
fn pairing_stores_a_private_token_that_the_daemon_uses_over_tcp() {
    let server = Server::start(&["Over TCP. "], true);
    let tcp = server.0.tcp_endpoint().unwrap();
    // A daemon first, so the pairing goes into its sandbox; it can't connect yet.
    let mut client = Client::start(tcp.clone());
    let out = client.psst(&["pair", &tcp.to_string(), "WRONGCODE"]);
    assert!(!out.status.success());
    client.ok(&["pair", &tcp.to_string(), "PAIRCODE"]);
    let creds = client.daemon().state_dir().join("credentials");
    assert_eq!(fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    let stored: Value = serde_json::from_slice(&fs::read(&creds).unwrap()).unwrap();
    assert_eq!(stored["token"], "fake-token");

    client.restart();
    client.ok(&["toggle"]);
    thread::sleep(Duration::from_millis(300));
    client.ok(&["toggle"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("paste"), vec!["Over TCP. "]);
}

#[test]
fn commands_fail_clearly_without_a_daemon() {
    let sandbox = Sandbox::headless();
    let out = sandbox.command(DAEMON).arg("toggle").output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("psst daemon"), "{stderr}");
}

#[test]
fn a_second_daemon_refuses_to_start() {
    let server = Server::start(&[], false);
    let client = Client::start(server.endpoint());
    let out = client.sandbox.command(DAEMON).arg("daemon").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("another psst daemon"));
}
