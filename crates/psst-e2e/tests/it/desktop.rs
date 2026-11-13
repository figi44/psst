use crate::support;

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use psst_config::{AppRule, ClientConfig, PasteAction};
use psst_engine_proto::fake::{LoggedRequest, Script, TranscribeStep};
use psst_proto::Endpoint;
use psst_testkit::{
    RealEngine, Sandbox, TestClientConfig, TestDaemon, TestServer, TestServerConfig, nested_niri, spawn_daemon,
    spawn_server, test_audio, wayland::ClipboardOwner, write_wav,
};
use serde_json::Value;
use support::{App, Proxy, Seen, Watcher, in_order, now_us};

const DAEMON: &str = env!("CARGO_BIN_EXE_psst-e2e-daemon");
const SERVER: &str = env!("CARGO_BIN_EXE_psst-server-e2e");
const WAIT: Duration = Duration::from_secs(15);
const ORIGINAL: &str = "copied before dictating ✓";
const RESTORE: Duration = Duration::from_secs(5);
const LOOK: Duration = Duration::from_millis(30);

/// The pipeline appends a space to each (the `trailing_space` rule).
fn server(texts: &[&str]) -> TestServer {
    let transcribe = texts
        .iter()
        .map(|text| TranscribeStep::Result {
            text: text.to_string(),
            delay_ms: 0,
            no_speech_prob: None,
            speech_ms: None,
        })
        .collect();
    let script = Script { transcribe, ..Script::default() };
    spawn_server(Path::new(SERVER), TestServerConfig { script, ..Default::default() })
}

struct Desktop {
    sandbox: Arc<Sandbox>,
    app: App,
    daemon: TestDaemon,
    _clipboard: ClipboardOwner,
}

impl Desktop {
    fn start(server: Endpoint, audio: &[i16]) -> Desktop {
        Desktop::start_with(server, audio, |_| {})
    }

    fn start_with(server: Endpoint, audio: &[i16], tweak: impl FnOnce(&mut ClientConfig)) -> Desktop {
        let sandbox = Arc::new(nested_niri());
        let app = App::start(&sandbox);
        let clipboard = ClipboardOwner::new(
            &sandbox,
            &[("text/plain;charset=utf-8", ORIGINAL.as_bytes()), ("text/plain", ORIGINAL.as_bytes())],
        );
        let wav = sandbox.root.path().join("clip.wav");
        write_wav(&wav, audio);
        let mut cfg = TestClientConfig::new(server, wav);
        tweak(&mut cfg.client);
        let daemon = spawn_daemon(Path::new(DAEMON), &sandbox, cfg);
        Desktop { sandbox, app, daemon, _clipboard: clipboard }
    }

    fn watch(&self) -> Watcher {
        let watcher = Watcher::start(self.sandbox.clone(), LOOK);
        watcher.wait_for(Seen::Hidden, Duration::from_secs(5));
        watcher
    }

    #[track_caller]
    fn ok(&self, args: &[&str]) -> String {
        let out = self.daemon.run(args);
        assert!(
            out.status.success(),
            "psst {args:?} failed: {}{}\ndaemon log:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            self.daemon.log()
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn status(&self) -> Value {
        serde_json::from_str(&self.ok(&["status", "--json"])).unwrap()
    }

    #[track_caller]
    fn wait_for(&self, what: &str, mut check: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let status = self.status();
            if check(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {status}\n{}", self.daemon.log());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_idle(&self) -> Value {
        self.wait_for("idle", |s| s["state"] == "idle")
    }

    #[track_caller]
    fn pasted(&self, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = self.app.pending().into_iter().find(|v| v["ev"] == "paste") {
                return v;
            }
            assert!(Instant::now() < deadline, "nothing pasted within {timeout:?}; daemon log:\n{}", self.daemon.log());
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn recordings(&self) -> usize {
        fs::read_dir(self.daemon.state_dir().join("recordings")).map(Iterator::count).unwrap_or(0)
    }

    fn last_json(&self) -> Value {
        serde_json::from_slice(&fs::read(self.daemon.state_dir().join("last.json")).unwrap()).unwrap()
    }
}

#[track_caller]
fn assert_pasted(got: &Value, text: &str) {
    assert_eq!(got["text"].as_str(), Some(text), "the app got {got}");
    assert_eq!(got["chord"], "ctrl+v", "niri reports psst-paste-target, which gets Ctrl+V");
}

fn transcriptions(server: &TestServer) -> Vec<LoggedRequest> {
    server.engine_requests().into_iter().filter(|r| r.header["type"] == "transcribe").collect()
}

/// `recorded` runs from the first `toggle`'s reply (the daemon starts the recorder before it
/// answers) to sending the second, so the slack covers only the first frame's wait: timing the
/// commands too would count their start-up, which takes longer than that on a loaded machine.
#[track_caller]
fn assert_whole(request: &LoggedRequest, recorded: Duration) {
    let ms = request.samples as u128 / 16;
    assert!(ms + 200 >= recorded.as_millis(), "{ms} ms of audio from a {recorded:?} recording");
}

fn timings(status: &Value) -> Value {
    status["last_timings"].clone()
}

#[test]
fn a_dictation_is_pasted_into_the_focused_app_and_the_clipboard_comes_back() {
    let server = server(&["Push the fix to psst."]);
    let mut desk = Desktop::start(server.endpoint(), &test_audio(3_000));
    let mut watcher = desk.watch();

    assert!(desk.ok(&["toggle"]).starts_with("recording"));
    watcher.wait_for(Seen::Recording, Duration::from_secs(3));
    thread::sleep(Duration::from_millis(600));
    let stopping_us = now_us();
    assert!(desk.ok(&["toggle"]).starts_with("stopped"));
    let got = desk.pasted(Duration::from_secs(5));
    let pasted_at = Instant::now();
    assert_pasted(&got, "Push the fix to psst. ");

    let status = desk.wait_idle();
    assert_eq!(desk.recordings(), 0, "the delivered recording is deleted");
    assert_eq!(desk.last_json()["text"], "Push the fix to psst. ");
    assert_eq!(desk.app.probe()["text"].as_str(), Some("Push the fix to psst. "));
    watcher.wait_for(Seen::Done, Duration::from_secs(3));
    watcher.wait_for(Seen::Hidden, Duration::from_secs(3));
    let seen = watcher.stop();
    assert!(in_order(&seen, &[Seen::Hidden, Seen::Recording, Seen::Done, Seen::Hidden]), "the overlay showed {seen:?}");

    // Not before 5 s. The app's clock says when it had read the clipboard, counted from before
    // the stop, so a probe that comes late on a loaded machine checks nothing rather than taking
    // the restore for an early one...
    thread::sleep((pasted_at + Duration::from_secs(4)).saturating_duration_since(Instant::now()));
    let probe = desk.app.probe();
    let probed_after = Duration::from_micros(probe["t_done"].as_u64().unwrap() - stopping_us);
    if probed_after < RESTORE {
        assert_eq!(probe["text"].as_str(), Some("Push the fix to psst. "), "restored {probed_after:?} after the stop");
    }
    // ...but after.
    thread::sleep((pasted_at + RESTORE + Duration::from_millis(500)).saturating_duration_since(Instant::now()));
    assert_eq!(desk.app.probe()["text"].as_str(), Some(ORIGINAL), "the clipboard is restored after 5 s");

    let t = timings(&status);
    eprintln!("one dictation in the sandbox (ms): {t}");
    for key in ["toggle_to_overlay_ms", "toggle_to_first_frame_ms", "stop_to_final_ms", "stop_to_dispatch_ms"] {
        assert!(t[key].is_u64(), "{key} in {t}");
    }
}

#[test]
fn the_focused_apps_rule_from_niri_picks_the_chord() {
    let server = server(&["In a terminal."]);
    let desk = Desktop::start_with(server.endpoint(), &test_audio(3_000), |cfg| {
        let rule = AppRule { paste: Some(PasteAction::CtrlShiftV), paste_delay_ms: 0 };
        cfg.apps.insert("psst-paste-target".into(), rule);
    });
    desk.ok(&["toggle"]);
    thread::sleep(Duration::from_millis(400));
    desk.ok(&["toggle"]);
    let got = desk.pasted(Duration::from_secs(5));
    assert_eq!((got["text"].as_str(), got["chord"].as_str()), (Some("In a terminal. "), Some("ctrl+shift+v")), "{got}");
}

#[test]
fn a_connection_dropped_while_recording_is_uploaded_with_resume_at_stop() {
    let server = server(&["After the drop."]);
    let dir = tempfile::Builder::new().prefix("psst-proxy.").tempdir_in("/tmp").unwrap();
    let proxy = Proxy::start(&dir.path().join("server.sock"), server.socket());
    let desk = Desktop::start(Endpoint::Unix(proxy.path().to_owned()), &test_audio(3_000));
    let mut watcher = desk.watch();

    desk.ok(&["toggle"]);
    let recording = Instant::now();
    watcher.wait_for(Seen::Recording, Duration::from_secs(3));
    // Cut only once the server has the session.
    server.wait_for_log(": recording", 1, WAIT);
    thread::sleep(Duration::from_millis(500));
    let cut = Instant::now();
    proxy.cut();
    let offline = watcher.wait_for(Seen::Offline, Duration::from_secs(3));
    eprintln!("connection lost while recording → offline seen on screen: {:?}", offline - cut);

    thread::sleep(Duration::from_millis(500));
    let recorded = recording.elapsed();
    desk.ok(&["toggle"]);
    assert_pasted(&desk.pasted(Duration::from_secs(10)), "After the drop. ");
    desk.wait_idle();
    assert_eq!(desk.recordings(), 0);
    let log = desk.daemon.log();
    assert!(log.contains("is offline") && log.contains("uploading"), "{log}");
    watcher.wait_for(Seen::Done, Duration::from_secs(3));
    watcher.wait_for(Seen::Hidden, Duration::from_secs(3));
    let seen = watcher.stop();
    assert!(
        in_order(&seen, &[Seen::Recording, Seen::Offline, Seen::Done, Seen::Hidden]),
        "the overlay showed {seen:?}"
    );
    let requests = transcriptions(&server);
    assert_eq!(requests.len(), 1, "one transcription, from the upload");
    assert_whole(&requests[0], recorded);
}

#[test]
fn dictation_timings() {
    // Enough to catch a dictation that's slower than the first; with this few, p95 is the max.
    const RUNS: usize = 3;
    let server = server(&["Timing."; RUNS]);
    let desk = Desktop::start(server.endpoint(), &test_audio(3_000));
    let mut runs = Vec::new();
    for run in 0..RUNS {
        if run > 0 {
            // Let `Done` go, so this toggle shows the overlay again.
            thread::sleep(Duration::from_millis(1_200));
        }
        desk.ok(&["toggle"]);
        thread::sleep(Duration::from_millis(500));
        desk.ok(&["toggle"]);
        assert_pasted(&desk.pasted(Duration::from_secs(5)), "Timing. ");
        runs.push(timings(&desk.wait_idle()));
    }
    let mut summary = Vec::new();
    for key in ["toggle_to_overlay_ms", "toggle_to_first_frame_ms", "stop_to_final_ms", "stop_to_dispatch_ms"] {
        let mut ms: Vec<u64> = runs.iter().map(|t| t[key].as_u64().unwrap_or_else(|| panic!("{key} in {t}"))).collect();
        ms.sort();
        let at = |p: f64| ms[((ms.len() - 1) as f64 * p).round() as usize];
        summary.push((key, at(0.5), at(0.95), ms[ms.len() - 1]));
    }
    for (key, p50, p95, max) in &summary {
        eprintln!("{key} over {RUNS} dictations (fake engine, nested niri): p50 {p50}, p95 {p95}, max {max}");
    }
    // Generous bounds for loaded CI machines.
    let p95 = |key: &str| summary.iter().find(|s| s.0 == key).unwrap().2;
    assert!(p95("toggle_to_overlay_ms") < 500, "{summary:?}");
    assert!(p95("toggle_to_first_frame_ms") < 500, "{summary:?}");
    assert!(p95("stop_to_dispatch_ms") < 1_000, "{summary:?}");
}

const GPU_CLIP: (&str, &str) =
    ("syn-en-181-lj", "Please write down the steps you took, so we can reproduce the crash tomorrow.");

#[test]
#[ignore = "GPU: runs inside scripts/gpu-session.sh with nix develop .#gpu"]
fn on_the_gpu() {
    assert!(
        env::var_os("PSST_GPU_SESSION").is_some(),
        "the GPU run uses the GPU, so it runs only inside scripts/gpu-session.sh"
    );
    let engine = which("psst-engine-dev").expect("psst-engine-dev isn't on PATH: run in `nix develop .#gpu`");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let audio = read_flac(&repo.join("evals/clips").join(format!("{}.flac", GPU_CLIP.0)));
    let clip = Duration::from_millis(audio.len() as u64 / 16);

    let server = spawn_server(
        Path::new(SERVER),
        TestServerConfig {
            real_engine: Some(RealEngine { command: vec![engine.display().to_string()], model_cache: model_cache() }),
            ..Default::default()
        },
    );
    let loading = Instant::now();
    let out = Command::new(SERVER).args(["model", "load", "--socket"]).arg(server.socket()).output().unwrap();
    assert!(out.status.success(), "model load: {}\n{}", String::from_utf8_lossy(&out.stderr), server.log());
    eprintln!("GPU: the model loaded in {:?}", loading.elapsed());

    let desk = Desktop::start(server.endpoint(), &audio);
    for run in 1..=3 {
        desk.ok(&["toggle"]);
        thread::sleep(clip + Duration::from_millis(400));
        desk.ok(&["toggle"]);
        let got = desk.pasted(Duration::from_secs(60));
        let status = desk.wait_idle();
        let text = got["text"].as_str().unwrap_or_default().to_owned();
        let t = timings(&status);
        eprintln!("GPU run {run}: {text:?}\n  timings (ms): {t}");
        let (want, have) = (words(GPU_CLIP.1), words(&text));
        let found = want.iter().filter(|w| have.contains(w)).count();
        assert!(found * 10 >= want.len() * 8, "{text:?} is far from {:?}", GPU_CLIP.1);
        assert!(t["stop_to_dispatch_ms"].is_u64(), "{t}");
        thread::sleep(Duration::from_millis(1_200));
    }
    assert_eq!(desk.recordings(), 0);
}

fn which(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(program)).find(|path| path.is_file())
}

/// The model cache `psst-eval audio` uses.
fn model_cache() -> PathBuf {
    let cache = env::var_os("PSST_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|| {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").expect("HOME")).join(".cache"))
            .join("psst")
    });
    cache.join("hf")
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

fn read_flac(path: &Path) -> Vec<i16> {
    let mut reader = claxon::FlacReader::open(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let info = reader.streaminfo();
    assert_eq!((info.sample_rate, info.channels), (16_000, 1), "{}", path.display());
    let bits = info.bits_per_sample;
    reader
        .samples()
        .map(|s| {
            let s = s.expect("decode the clip");
            if bits >= 16 { (s >> (bits - 16)) as i16 } else { (s << (16 - bits)) as i16 }
        })
        .collect()
}
