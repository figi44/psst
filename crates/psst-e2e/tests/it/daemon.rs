use std::{
    fs,
    io::{BufRead, BufReader, Read},
    net::{IpAddr, Ipv4Addr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use psst_engine_proto::fake::{Script, TranscribeStep};
use psst_proto::Endpoint;
use psst_testkit::{
    Sandbox, TestClientConfig, TestDaemon, TestServer, TestServerConfig,
    fake_tailscale::{FakeTailscale, TAILNET},
    spawn_daemon, spawn_server, test_audio, write_wav,
};
use serde_json::Value;

const DAEMON: &str = env!("CARGO_BIN_EXE_psst-e2e-daemon");
const SERVER: &str = env!("CARGO_BIN_EXE_psst-server-e2e");
const WAIT: Duration = Duration::from_secs(15);

struct Server(TestServer);

impl Server {
    fn start(texts: &[&str], tcp: bool) -> Server {
        Self::start_with(texts, |cfg| cfg.tcp = tcp)
    }

    /// The pipeline appends a space to each (the `trailing_space` rule).
    fn start_with(texts: &[&str], adjust: impl FnOnce(&mut TestServerConfig)) -> Server {
        let transcribe = texts
            .iter()
            .map(|text| TranscribeStep::Result {
                text: text.to_string(),
                delay_ms: 0,
                no_speech_prob: None,
                speech_ms: None,
            })
            .collect();
        let mut cfg = TestServerConfig { script: Script { transcribe, ..Script::default() }, ..Default::default() };
        adjust(&mut cfg);
        Server(spawn_server(Path::new(SERVER), cfg))
    }

    fn endpoint(&self) -> Endpoint {
        self.0.endpoint()
    }

    fn stop(&mut self) {
        self.0.kill();
    }

    fn start_again(&mut self) {
        self.0.restart();
    }

    /// Across engine restarts.
    fn transcriptions(&self) -> usize {
        self.0.engine_requests().iter().filter(|r| r.header["type"] == "transcribe").count()
    }

    fn admin(&self, args: &[&str]) -> Value {
        let out = Command::new(SERVER).args(args).arg("--json").arg("--socket").arg(self.0.socket()).output().unwrap();
        assert!(out.status.success(), "psst-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
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

    fn commands(&self) -> Vec<Value> {
        let text = fs::read_to_string(&self.inserted).unwrap_or_default();
        // A line the daemon is still writing has no newline yet; leave it for the next look.
        let complete = text.rfind('\n').map_or("", |end| &text[..end]);
        complete.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
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
    let server = Server::start(&["Push the fix to psst."], false);
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
    let deadline = Instant::now() + WAIT;
    loop {
        let history = server.admin(&["history"]);
        if history[0]["client_timings"]["stop_to_dispatch_ms"].is_u64() {
            break;
        }
        assert!(Instant::now() < deadline, "no client timings in {history}");
        thread::sleep(Duration::from_millis(50));
    }

    // `paste-last` pastes it again without transcribing it again.
    assert_eq!(client.ok(&["paste-last"]).trim(), "pasting");
    client.wait_for("the second paste", |c| c.inserted_texts("paste").len() == 2);
    assert_eq!(client.inserted_texts("paste"), vec!["Push the fix to psst. "; 2]);
    assert_eq!(server.transcriptions(), 1);
}

#[test]
fn a_recording_made_offline_is_uploaded_once_the_server_is_back() {
    let mut server = Server::start(&["Recorded offline."], false);
    let client = Client::start(server.endpoint());
    server.stop();
    client.ok(&["toggle"]);
    client.wait_for("the offline overlay", |c| {
        c.commands().iter().any(|cmd| cmd["cmd"] == "overlay" && cmd["state"]["kind"] == "offline")
    });
    let status = client.status();
    let phase = status["recording"]["phase"].as_str().unwrap_or_default();
    assert!(phase.starts_with("recording, offline"), "{status}");
    thread::sleep(Duration::from_millis(300));
    server.start_again();
    client.ok(&["toggle"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("paste"), vec!["Recorded offline. "]);
    let recordings = client.daemon().state_dir().join("recordings");
    assert_eq!(fs::read_dir(recordings).unwrap().count(), 0, "the delivered recording is deleted");
    assert_eq!(server.transcriptions(), 1);
}

#[test]
fn a_recording_interrupted_by_a_crash_survives_for_retry() {
    let mut server = Server::start(&["Rescued."], false);
    let mut client = Client::start(server.endpoint());
    server.stop();
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

    server.start_again();
    client.ok(&["retry"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("copy"), vec!["Rescued. "], "an incomplete recording is copied, not pasted");
    assert!(client.inserted_texts("paste").is_empty());
}

#[test]
fn settings_changed_in_the_web_ui_reach_the_running_daemon() {
    use psst_client::api::{ApiClient, Method};
    use psst_proto::api::routes;

    let server = Server::start(&[], false);
    let client = Client::start(server.endpoint());
    client
        .wait_for("the connection", |c| c.status()["connection"].as_str().is_some_and(|s| s.starts_with("connected")));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let api = ApiClient::new(server.endpoint(), None);
    let devices: Vec<Value> = runtime.block_on(api.call(Method::Get, routes::DEVICES, None::<&()>)).unwrap();
    let device = devices.iter().find(|d| d["reports_config"] == true).expect("the daemon sent its client.toml");
    let id = device["id"].as_str().unwrap();
    let view: psst_proto::api::DeviceSettingsView =
        runtime.block_on(api.call(Method::Get, &routes::with_id(routes::DEVICE_SETTINGS, id), None::<&()>)).unwrap();
    assert_eq!(view.settings.field("sounds").unwrap().file, Some("off".into()), "the daemon's own client.toml");

    let set = |key: &str, value: &str| {
        let path = routes::with(&routes::with_id(routes::DEVICE_SETTING, id), "key", key);
        runtime.block_on(api.call::<Value>(Method::Put, &path, Some(&serde_json::json!({"value": value})))).unwrap();
    };
    set("post_processing", "light");
    set("overlay.accent", "#00ff00");
    client.wait_for("the new post-processing", |c| c.status()["post_processing"] == "light");
    client.wait_for("the new overlay", |c| {
        c.commands().iter().any(|cmd| cmd["cmd"] == "configure" && cmd["overlay"]["accent"] == "#00ff00ff")
    });
}

#[test]
fn pairing_stores_a_private_token_that_the_daemon_uses_over_tcp() {
    let server = Server::start(&["Over TCP."], true);
    let tcp = server.0.tcp_endpoint().unwrap();
    // A daemon first, so the pairing goes into its sandbox; it can't connect yet.
    let mut client = Client::start(tcp.clone());
    let out = client.psst(&["pair", &tcp.to_string(), "WRONG234"]);
    assert!(!out.status.success());
    let code = server.admin(&["pair", "--name", "work-laptop"])["code"].as_str().unwrap().to_owned();
    client.ok(&["pair", &tcp.to_string(), &code]);
    let creds = client.daemon().state_dir().join("credentials");
    assert_eq!(fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    let stored: Value = serde_json::from_slice(&fs::read(&creds).unwrap()).unwrap();
    assert!(stored["token"].as_str().unwrap().starts_with("psst_"), "{stored}");

    client.restart();
    client.ok(&["toggle"]);
    thread::sleep(Duration::from_millis(300));
    client.ok(&["toggle"]);
    client.wait_idle();
    assert_eq!(client.inserted_texts("paste"), vec!["Over TCP. "]);
}

#[test]
fn a_machine_in_auto_pair_pairs_itself_and_again_once_revoked() {
    let tailscale = FakeTailscale::start();
    tailscale.add(IpAddr::V4(Ipv4Addr::LOCALHOST), "nThinkpadCNTRL", &format!("thinkpad.{TAILNET}"));
    let server = Server::start_with(&["Paired by itself.", "Paired again."], |cfg| {
        cfg.tcp = true;
        cfg.server.auto_pair = vec!["thinkpad".into()];
        cfg.server.tailscale_socket = tailscale.socket().into();
    });
    let client = Client::start(server.0.tcp_endpoint().unwrap());
    let dictate = |client: &Client| {
        client.ok(&["toggle"]);
        thread::sleep(Duration::from_millis(300));
        client.ok(&["toggle"]);
        client.wait_idle();
    };
    let creds = client.daemon().state_dir().join("credentials");
    let device = || -> String {
        let stored: Value = serde_json::from_slice(&fs::read(&creds).unwrap()).unwrap();
        stored["device_id"].as_str().unwrap().to_owned()
    };

    dictate(&client);
    assert_eq!(client.inserted_texts("paste"), vec!["Paired by itself. "]);
    assert_eq!(fs::metadata(&creds).unwrap().permissions().mode() & 0o777, 0o600);
    let first = device();

    let revoke = Command::new(SERVER).args(["devices", "revoke", &first, "--socket"]).arg(server.0.socket()).output();
    assert!(revoke.unwrap().status.success());
    dictate(&client);
    assert_eq!(client.inserted_texts("paste"), vec!["Paired by itself. ", "Paired again. "]);
    assert_ne!(device(), first);
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
fn a_second_daemon_refuses_to_start_and_leaves_the_recordings_alone() {
    let server = Server::start(&[], false);
    let client = Client::start(server.endpoint());
    client.ok(&["toggle"]);
    let out = client.sandbox.command(DAEMON).arg("daemon").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("another psst daemon"));
    let status = client.status();
    assert_eq!(status["retained"][0]["state"], "recording", "not recovered under the running daemon: {status}");
}

#[test]
fn status_follow_exits_quietly_when_its_output_closes() {
    // Waiting to retry without a daemon, and following an idle daemon: neither has anything to
    // write, so it has to notice the bar going away by itself.
    let sandbox = Sandbox::headless();
    exits_once_its_first_line_is_read(sandbox.command(DAEMON), "stopped");
    let server = Server::start(&[], false);
    let client = Client::start(server.endpoint());
    exits_once_its_first_line_is_read(client.sandbox.command(DAEMON), "idle");
}

#[track_caller]
fn exits_once_its_first_line_is_read(mut command: Command, alt: &str) {
    let mut child = command.args(["status", "--follow"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, first) = mpsc::channel();
    // Reads one line, then drops the pipe's only reader.
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line: Value = serde_json::from_str(&first.recv_timeout(WAIT).expect("a first line")).unwrap();
    assert_eq!(line["alt"], alt, "{line}");
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "still running after its output closed");
        thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(status.success() && stderr.is_empty(), "{status}: {stderr}");
}
