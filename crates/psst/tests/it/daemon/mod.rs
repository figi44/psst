mod follow;
mod incompatible;
mod slow_link;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use psst::{
    audio::FileSource,
    daemon::{self, DaemonHandle, Deps, Paths, Timing},
    ipc::{Request, Response, Status},
    store::{Last, Meta, RecState, Store},
    ui::{Wayland, WaylandEvents, WaylandSink},
};
use psst_config::{AppRule, ClientConfig, PasteAction};
use psst_proto::{ErrorCode, SessionId};
use psst_testkit::{
    fake_server::{FakeServer, FakeServerConfig, Outcome},
    test_audio, write_wav,
};
use psst_wayland::{Chord, OverlayState, WaylandCmd, WaylandEvent};

const WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct FakeWayland {
    cmds: Arc<Mutex<Vec<WaylandCmd>>>,
    fail_inserts: Arc<AtomicBool>,
    shown_after_ms: Arc<AtomicU64>,
}

struct FakeSink {
    wayland: FakeWayland,
    events: WaylandEvents,
    visible: bool,
}

impl Wayland for FakeWayland {
    fn connect(&mut self, events: WaylandEvents) -> Result<Box<dyn WaylandSink>, String> {
        Ok(Box::new(FakeSink { wayland: self.clone(), events, visible: false }))
    }
}

impl WaylandSink for FakeSink {
    fn send(&mut self, cmd: WaylandCmd) {
        self.wayland.cmds.lock().unwrap().push(cmd.clone());
        let fail = self.wayland.fail_inserts.load(Ordering::SeqCst);
        let reply = match cmd {
            WaylandCmd::Overlay(Some(_)) if !self.visible => {
                self.visible = true;
                Some(WaylandEvent::OverlayShown)
            }
            WaylandCmd::Overlay(state) => {
                self.visible = state.is_some();
                None
            }
            WaylandCmd::Paste { generation, .. } | WaylandCmd::Copy { generation, .. } if fail => {
                Some(WaylandEvent::Failed { generation, reason: "the app ignored it".into() })
            }
            WaylandCmd::Paste { generation, .. } => Some(WaylandEvent::Pasted { generation }),
            WaylandCmd::Copy { generation, .. } => Some(WaylandEvent::Copied { generation }),
            WaylandCmd::SnapshotClipboard { .. } => None,
        };
        let delay = Duration::from_millis(self.wayland.shown_after_ms.load(Ordering::SeqCst));
        match reply {
            Some(WaylandEvent::OverlayShown) if !delay.is_zero() => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let _ = events.send(WaylandEvent::OverlayShown);
                });
            }
            Some(event) => {
                let _ = self.events.send(event);
            }
            None => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Inserted {
    Paste(String, Chord),
    Copy(String),
}

impl FakeWayland {
    fn inserted(&self) -> Vec<Inserted> {
        self.cmds
            .lock()
            .unwrap()
            .iter()
            .filter_map(|cmd| match cmd {
                WaylandCmd::Paste { text, chord, .. } => Some(Inserted::Paste(text.clone(), *chord)),
                WaylandCmd::Copy { text, .. } => Some(Inserted::Copy(text.clone())),
                _ => None,
            })
            .collect()
    }

    fn overlays(&self) -> Vec<Option<OverlayState>> {
        self.cmds
            .lock()
            .unwrap()
            .iter()
            .filter_map(|cmd| if let WaylandCmd::Overlay(state) = cmd { Some(state.clone()) } else { None })
            .collect()
    }

    fn showed(&self, check: impl Fn(&OverlayState) -> bool) -> bool {
        self.overlays().iter().flatten().any(check)
    }

    fn snapshots_precede_inserts(&self) -> bool {
        let cmds = self.cmds.lock().unwrap();
        let mut snapshots = Vec::new();
        for cmd in cmds.iter() {
            match cmd {
                WaylandCmd::SnapshotClipboard { generation } => snapshots.push(*generation),
                WaylandCmd::Paste { generation, .. } | WaylandCmd::Copy { generation, .. } => {
                    if !snapshots.contains(generation) {
                        return false;
                    }
                }
                WaylandCmd::Overlay(_) => {}
            }
        }
        true
    }
}

struct Daemon {
    handle: Option<DaemonHandle>,
    socket: PathBuf,
    wayland: FakeWayland,
    sounds: Arc<AtomicUsize>,
    store: Store,
    audio: Vec<i16>,
    _dir: Option<tempfile::TempDir>,
}

async fn daemon(server: &FakeServer, tweak: impl FnOnce(&mut ClientConfig, &mut Deps)) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let mut d = daemon_in(dir.path(), server, tweak).await;
    d._dir = Some(dir);
    d
}

async fn daemon_in(root: &Path, server: &FakeServer, tweak: impl FnOnce(&mut ClientConfig, &mut Deps)) -> Daemon {
    let audio = test_audio(3_000);
    let wav = root.join("input.wav");
    write_wav(&wav, &audio);
    let wayland = FakeWayland::default();
    let sounds = Arc::new(AtomicUsize::new(0));
    let counter = sounds.clone();
    let mut cfg = ClientConfig { server: server.endpoint(), sounds: true, ..ClientConfig::default() };
    let mut deps = Deps {
        audio: Arc::new(FileSource { path: wav }),
        wayland: Box::new(wayland.clone()),
        sound: Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        niri_socket: None,
        timing: Timing::default(),
    };
    tweak(&mut cfg, &mut deps);
    let paths = Paths { state_dir: root.join("state"), socket: root.join("runtime/daemon.sock") };
    let socket = paths.socket.clone();
    let handle = daemon::spawn(cfg, paths, deps).await.unwrap();
    wait_until(|| server.connections() > 0).await;
    Daemon {
        handle: Some(handle),
        socket,
        wayland,
        sounds,
        store: Store::open(&root.join("state")).unwrap(),
        audio,
        _dir: None,
    }
}

impl Daemon {
    async fn run(&self, request: Request) -> Response {
        self.handle.as_ref().unwrap().request(request).await
    }

    async fn toggle(&self) -> Response {
        self.run(Request::Toggle { profile: None }).await
    }

    async fn status(&self) -> Status {
        self.run(Request::Status).await.status.unwrap()
    }

    async fn idle(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if self.status().await.state == "idle" {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "not idle: {:?}", self.status().await);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn dictate(&self, ms: u64) -> SessionId {
        let started = self.toggle().await;
        assert!(started.ok, "{started:?}");
        let id = started.message.rsplit(' ').next().unwrap().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let stopped = self.toggle().await;
        assert!(stopped.ok, "{stopped:?}");
        id
    }

    async fn stop(mut self) {
        self.handle.take().unwrap().shutdown().await;
    }
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn paste(text: &str) -> Inserted {
    Inserted::Paste(text.into(), Chord::CtrlV)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dictation_streams_live_and_is_pasted_then_forgotten() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("Push the fix. ".into()));
    let d = daemon(&server, |_, _| {}).await;

    let id = d.dictate(400).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste("Push the fix. ")]);
    assert!(d.wayland.snapshots_precede_inserts());
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1, "one live session");
    assert!(!seen.starts[0].resume);
    let samples = seen.stop_samples.unwrap() as usize;
    assert!(samples >= 6_000, "{samples}");
    assert_eq!(&seen.audio[..samples], &d.audio[..samples], "the server got the microphone's audio in order");
    assert!(d.store.list().is_empty());
    assert_eq!(d.store.read_last().unwrap().text, "Push the fix. ");
    wait_until(|| server.reports().iter().any(|r| r.session_id == id)).await;
    let timings = server.reports()[0].timings;
    assert!(timings.stop_to_dispatch_ms.is_some() && timings.toggle_to_first_frame_ms.is_some(), "{timings:?}");
    assert!(timings.toggle_to_overlay_ms.is_some());
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Starting)));
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Recording { .. })));
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Done)));
    assert_eq!(d.sounds.load(Ordering::SeqCst), 0);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toggle_to_overlay_is_timed_only_when_the_toggle_shows_the_overlay() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("timed ".into()));
    server.push_outcome(Outcome::Final("untimed ".into()));
    let d = daemon(&server, |_, _| {}).await;
    // The real thread reports `OverlayShown` once the frame is presented, a little later.
    d.wayland.shown_after_ms.store(300, Ordering::SeqCst);
    let first = d.dictate(500).await;
    d.idle().await;
    wait_until(|| d.wayland.overlays().last() == Some(&None)).await;

    // `paste-last` brings the overlay up and the toggle comes before it's presented: that
    // `OverlayShown` belongs to `paste-last`.
    assert!(d.run(Request::PasteLast { session: None }).await.ok);
    let second = d.dictate(500).await;
    d.idle().await;

    wait_until(|| server.reports().len() == 2).await;
    let timings = |id| server.reports().iter().find(|r| r.session_id == id).unwrap().timings;
    let shown = timings(first).toggle_to_overlay_ms.expect("the first toggle showed the overlay");
    assert!(shown >= 300, "{shown} ms");
    assert_eq!(timings(second).toggle_to_overlay_ms, None, "the overlay was already up");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toggling_while_finishing_records_again_at_once_and_pastes_in_order() {
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(700), ..Default::default() });
    server.push_outcome(Outcome::Final("first ".into()));
    server.push_outcome(Outcome::Final("second ".into()));
    let d = daemon(&server, |_, _| {}).await;

    let first = d.dictate(300).await;
    let second = d.dictate(300).await;
    assert_ne!(first, second);
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste("first "), paste("second ")]);
    // The second session's frames from before its `ready` arrived intact and first.
    let seen = server.session(second).unwrap();
    let samples = seen.stop_samples.unwrap() as usize;
    assert_eq!(&seen.audio[..samples], &d.audio[..samples]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_that_waited_for_the_connection_streams_its_early_frames_first() {
    let server = FakeServer::start(FakeServerConfig {
        final_delay: Duration::from_millis(300),
        ready_delay: Duration::from_millis(150),
        ..Default::default()
    });
    server.push_outcome(Outcome::Final("first ".into()));
    server.push_outcome(Outcome::Final("second ".into()));
    let d = daemon(&server, |_, _| {}).await;

    d.dictate(200).await;
    // Its `start` waits for the first's `final`, then goes live with the frames captured meanwhile.
    let second = d.dictate(900).await;
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste("first "), paste("second ")]);
    let seen = server.session(second).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert_eq!((seen.starts[0].mode, seen.starts[0].resume), (psst_proto::ws::Mode::Live, false), "streamed live");
    let samples = seen.stop_samples.unwrap() as usize;
    assert!(samples >= 12_800, "{samples}");
    assert_eq!(&seen.audio[..samples], &d.audio[..samples], "early frames first, in order");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toggling_while_starting_stops_once_recording_runs() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("short ".into()));
    let d = daemon(&server, |_, _| {}).await;
    assert!(d.toggle().await.ok);
    let second = d.toggle().await;
    assert!(second.ok, "{second:?}");
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("short ")]);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_shows_offline_at_once_and_the_recording_is_uploaded_at_stop() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("from the file ".into()));
    let d = daemon(&server, |_, _| {}).await;
    server.set_listening(false);

    let started = std::time::Instant::now();
    let id = {
        let r = d.toggle().await;
        r.message.rsplit(' ').next().unwrap().parse::<SessionId>().unwrap()
    };
    wait_until(|| d.wayland.showed(|s| matches!(s, OverlayState::Offline { .. }))).await;
    assert!(started.elapsed() < Duration::from_millis(1000), "offline after {:?}", started.elapsed());
    assert_eq!(d.sounds.load(Ordering::SeqCst), 1, "the error sound");
    tokio::time::sleep(Duration::from_millis(400)).await;
    server.set_listening(true);
    assert!(d.toggle().await.ok);
    d.idle().await;

    assert_eq!(d.wayland.inserted(), vec![paste("from the file ")]);
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 1);
    assert!(seen.starts[0].resume, "uploaded with its own id and resume");
    assert_eq!(seen.starts[0].mode, psst_proto::ws::Mode::Upload);
    let samples = seen.stop_samples.unwrap() as usize;
    assert!(samples >= 6_400, "the whole recording, from before the server came back: {samples}");
    assert_eq!(&seen.audio[..samples], &d.audio[..samples]);
    assert!(d.store.list().is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_connection_after_stop_the_recording_is_saved_for_retry() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("retried ".into()));
    let d = daemon(&server, |_, deps| deps.timing.stop_window = Duration::from_millis(800)).await;
    server.set_listening(false);

    let id = d.dictate(300).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Error { message } if message.contains("psst retry"))));
    let status = d.status().await;
    assert_eq!(status.retained.len(), 1);
    assert_eq!((status.retained[0].session_id, status.retained[0].state.as_str()), (id, "complete"));
    assert!(status.retained[0].hint.contains("psst retry"));

    server.set_listening(true);
    assert!(d.run(Request::Retry { session: None }).await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("retried ")]);
    assert!(server.session(id).unwrap().starts[0].resume);
    assert!(d.store.list().is_empty());
    assert!(d.wayland.snapshots_precede_inserts());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejection_at_start_keeps_recording_and_uploads_at_stop() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.reject_next(ErrorCode::Busy);
    server.push_outcome(Outcome::Final("after busy ".into()));
    let d = daemon(&server, |_, _| {}).await;
    let id = d.dictate(400).await;
    d.idle().await;
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Offline { reason, .. } if reason.contains("Busy"))));
    assert_eq!(d.wayland.inserted(), vec![paste("after busy ")]);
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts.len(), 2);
    assert!(seen.starts[1].resume);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_rejection_saves_the_recording() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.reject_next(ErrorCode::ModelUnloaded);
    server.reject_next(ErrorCode::ModelUnloaded);
    let d = daemon(&server, |_, _| {}).await;
    let id = d.dictate(300).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
    assert_eq!(d.store.read_meta(id).unwrap().state, RecState::Complete);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_connection_while_recording_goes_offline_and_uploads_at_stop() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("survived ".into()));
    let d = daemon(&server, |_, _| {}).await;
    let id = {
        let r = d.toggle().await;
        r.message.rsplit(' ').next().unwrap().parse::<SessionId>().unwrap()
    };
    wait_until(|| server.session(id).is_some_and(|s| !s.audio.is_empty())).await;
    server.set_stalled(true);
    wait_until(|| d.wayland.showed(|s| matches!(s, OverlayState::Offline { .. }))).await;
    server.set_stalled(false);
    assert!(d.toggle().await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("survived ")]);
    let seen = server.session(id).unwrap();
    assert!(seen.starts.last().unwrap().resume);
    let samples = seen.stop_samples.unwrap() as usize;
    assert_eq!(&seen.audio[..samples], &d.audio[..samples], "the upload has everything, gap included");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_discards_the_dictation_and_its_files() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, _| {}).await;
    let id = {
        let r = d.toggle().await;
        r.message.rsplit(' ').next().unwrap().parse::<SessionId>().unwrap()
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.run(Request::Cancel).await.ok);
    d.idle().await;
    wait_until(|| server.session(id).is_some_and(|s| s.cancelled)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(d.wayland.inserted().is_empty());
    assert!(d.store.list().is_empty());
    assert!(!d.store.wav_path(id).exists());
    assert!(!d.run(Request::Cancel).await.ok, "nothing left to cancel");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_final_pastes_nothing_and_leaves_last_json_alone() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("kept ".into()));
    server.push_outcome(Outcome::Final(String::new()));
    let d = daemon(&server, |_, _| {}).await;
    d.dictate(200).await;
    d.idle().await;
    let before = d.store.read_last().unwrap();
    d.dictate(200).await;
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("kept ")]);
    assert_eq!(d.store.read_last().unwrap(), before);
    assert!(d.store.list().is_empty());
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::NothingHeard)));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_paste_keeps_the_text_for_paste_last() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("again ".into()));
    let d = daemon(&server, |_, _| {}).await;
    d.wayland.fail_inserts.store(true, Ordering::SeqCst);
    let id = d.dictate(200).await;
    d.idle().await;
    let status = d.status().await;
    assert_eq!(status.retained.len(), 1);
    assert_eq!(status.retained[0].state, "transcribed");
    assert_eq!(status.retained[0].text.as_deref(), Some("again "));
    assert_eq!(status.retained[0].hint, format!("psst paste-last --session {id}"));
    assert!(d.sounds.load(Ordering::SeqCst) >= 1);

    d.wayland.fail_inserts.store(false, Ordering::SeqCst);
    assert!(d.run(Request::PasteLast { session: Some(id) }).await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted().last(), Some(&paste("again ")));
    assert!(d.store.list().is_empty(), "delivered at last");
    assert!(d.wayland.snapshots_precede_inserts());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn paste_last_and_copy_last_redeliver_the_last_text() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, _| {}).await;
    assert!(!d.run(Request::PasteLast { session: None }).await.ok, "nothing dictated yet");
    d.store
        .write_last(&Last {
            session_id: SessionId::new_v4(),
            finalized_at: jiff::Timestamp::now(),
            text: "old ".into(),
        })
        .unwrap();
    assert!(d.run(Request::PasteLast { session: None }).await.ok);
    assert!(d.run(Request::CopyLast { session: None }).await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![paste("old "), Inserted::Copy("old ".into())]);
    assert!(d.wayland.snapshots_precede_inserts());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chord_follows_the_focused_window_and_app_rules() {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };
    let dir = tempfile::tempdir().unwrap();
    let niri = dir.path().join("niri.sock");
    let listener = UnixListener::bind(&niri).unwrap();
    let app = Arc::new(Mutex::new("com.mitchellh.ghostty".to_string()));
    let serving = app.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let app_id = serving.lock().unwrap().clone();
            let reply = format!("{{\"Ok\":{{\"FocusedWindow\":{{\"id\":1,\"app_id\":\"{app_id}\"}}}}}}\n");
            write.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("terminal ".into()));
    server.push_outcome(Outcome::Final("secret ".into()));
    let d = daemon(&server, |cfg, deps| {
        deps.niri_socket = Some(niri.clone());
        // Real niri answers in milliseconds; a loaded CI runner can take longer than the default.
        deps.timing.niri_lookup = Duration::from_secs(5);
        cfg.apps.insert(
            "org.keepassxc.KeePassXC".into(),
            AppRule { paste: Some(PasteAction::CopyOnly), paste_delay_ms: 0 },
        );
    })
    .await;
    d.dictate(200).await;
    d.idle().await;
    *app.lock().unwrap() = "org.keepassxc.KeePassXC".into();
    d.dictate(200).await;
    d.idle().await;
    assert_eq!(
        d.wayland.inserted(),
        vec![Inserted::Paste("terminal ".into(), Chord::CtrlShiftV), Inserted::Copy("secret ".into())]
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_after_stop_keeps_the_recording_for_retry() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Error(ErrorCode::EngineFailed));
    let d = daemon(&server, |_, _| {}).await;
    let id = d.dictate(200).await;
    d.idle().await;
    assert!(d.wayland.inserted().is_empty());
    assert_eq!(d.store.read_meta(id).unwrap().state, RecState::Complete);
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::Error { .. })));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_at_start_marks_interrupted_recordings_and_retry_copies_them() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("rescued ".into()));
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state")).unwrap();
    let id = SessionId::new_v4();
    let meta = Meta {
        profile: "czech".into(),
        started_at: jiff::Timestamp::now(),
        samples: 0,
        gapped: false,
        state: RecState::Recording,
        final_text: None,
        finalized_at: None,
    };
    store.write_meta(id, &meta).unwrap();
    write_wav(&store.wav_path(id), &test_audio(500));

    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    let status = d.status().await;
    assert_eq!(status.retained.len(), 1);
    assert_eq!((status.retained[0].state.as_str(), status.retained[0].duration_ms), ("incomplete", 500));
    // Nothing is sent or pasted automatically.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(server.session(id).is_none());

    assert!(d.run(Request::Retry { session: Some(id) }).await.ok);
    d.idle().await;
    assert_eq!(d.wayland.inserted(), vec![Inserted::Copy("rescued ".into())], "incomplete: copied, not pasted");
    let seen = server.session(id).unwrap();
    assert_eq!(seen.starts[0].profile.as_deref(), Some("czech"));
    assert_eq!(seen.stop_samples, Some(8_000));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_when_recordings_are_over_budget() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, _| {}).await;
    let big = std::fs::File::create(d.store.recordings_dir().join("filler.bin")).unwrap();
    big.set_len(2_100_000_000).unwrap(); // sparse
    let refused = d.toggle().await;
    assert!(!refused.ok);
    assert!(refused.message.contains("psst status"), "{refused:?}");
    assert_eq!(d.status().await.state, "idle");
    assert!(server.sessions().is_empty());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recording_limit_warns_and_stops() {
    let server = FakeServer::start(FakeServerConfig { max_recording_s: 1, ..Default::default() });
    server.push_outcome(Outcome::Final("limited ".into()));
    let d = daemon(&server, |_, _| {}).await;
    let id = {
        let r = d.toggle().await;
        r.message.rsplit(' ').next().unwrap().parse::<SessionId>().unwrap()
    };
    d.idle().await;
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::LimitWarning { .. })));
    assert_eq!(d.wayland.inserted(), vec![paste("limited ")]);
    let samples = server.session(id).unwrap().stop_samples.unwrap();
    // Draining after the 1 s limit can add a few frames on a loaded machine; the server allows 5 s.
    assert!((16_000..24_000).contains(&samples), "stopped at the limit: {samples}");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_deletes_a_saved_recording() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, _| {}).await;
    let id = SessionId::new_v4();
    let meta = Meta {
        profile: "default".into(),
        started_at: jiff::Timestamp::now(),
        samples: 16,
        gapped: false,
        state: RecState::Complete,
        final_text: None,
        finalized_at: None,
    };
    d.store.write_meta(id, &meta).unwrap();
    write_wav(&d.store.wav_path(id), &[0; 16]);
    assert!(d.run(Request::Discard { session: id }).await.ok);
    assert!(d.store.list().is_empty());
    assert!(!d.run(Request::Discard { session: id }).await.ok);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_mid_recording_keeps_it_for_retry() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    let id = {
        let r = d.toggle().await;
        r.message.rsplit(' ').next().unwrap().parse::<SessionId>().unwrap()
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.stop().await;
    let store = Store::open(&dir.path().join("state")).unwrap();
    let meta = store.read_meta(id).unwrap();
    assert_eq!(meta.state, RecState::Complete);
    assert!(meta.samples >= 4_000, "{}", meta.samples);
    assert_eq!(psst::store::wav_samples(&store.wav_path(id)), meta.samples);
}
