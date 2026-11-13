use std::{
    io::{BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
};

use psst::follow::{self, Backoff, Format};
use serde_json::Value;

use super::*;

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    fn read_from(reader: impl Read + Send + 'static) -> Lines {
        let lines = Lines::default();
        let sink = lines.clone();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                sink.0.lock().unwrap().push(line);
            }
        });
        lines
    }

    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn statuses(&self) -> Vec<Status> {
        self.all().iter().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    fn bar(&self) -> Vec<Value> {
        self.all().iter().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    async fn wait(&self, what: &str, check: impl Fn(&Lines) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !check(self) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}; lines:\n{:#?}", self.all());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn subscribe(socket: &Path) -> (UnixStream, Lines) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.write_all(b"{\"cmd\":\"follow\"}\n").unwrap();
    let lines = Lines::read_from(stream.try_clone().unwrap());
    (stream, lines)
}

struct Cli {
    lines: Lines,
    out: UnixStream,
    done: mpsc::Receiver<std::io::Result<()>>,
}

impl Cli {
    fn start(socket: &Path, format: Format) -> Cli {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let (tx, done) = mpsc::channel();
        let socket = socket.to_path_buf();
        let backoff = Backoff { first: Duration::from_millis(50), max: Duration::from_millis(200) };
        thread::spawn(move || {
            let _ = tx.send(follow::run(&socket, format, &mut theirs, backoff));
        });
        Cli { lines: Lines::read_from(ours.try_clone().unwrap()), out: ours, done }
    }

    fn close(self) -> std::io::Result<()> {
        self.out.shutdown(Shutdown::Both).unwrap();
        self.done.recv_timeout(WAIT).expect("it returns once its output is closed")
    }
}

fn states(statuses: &[Status]) -> Vec<String> {
    let mut states: Vec<String> = statuses
        .iter()
        .map(|s| match s.state.as_str() {
            "starting" => "recording".to_string(),
            "inserting" => "finishing".to_string(),
            other => other.to_string(),
        })
        .collect();
    states.dedup();
    states
}

fn alts(lines: &[Value]) -> Vec<String> {
    let mut alts: Vec<String> = lines.iter().map(|l| l["alt"].as_str().unwrap().to_string()).collect();
    alts.dedup();
    alts
}

fn save(store: &Store, state: RecState, text: Option<String>) -> SessionId {
    let id = SessionId::new_v4();
    let meta = Meta {
        profile: "default".into(),
        started_at: jiff::Timestamp::now(),
        samples: 16,
        gapped: false,
        state,
        final_text: text,
        finalized_at: None,
        server: None,
    };
    store.write_meta(id, &meta).unwrap();
    write_wav(&store.wav_path(id), &[0; 16]);
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn every_follower_sees_each_transition_of_a_dictation() {
    let server = FakeServer::start(FakeServerConfig { final_delay: Duration::from_millis(500), ..Default::default() });
    server.push_outcome(Outcome::Final("Followed. ".into()));
    let d = daemon(&server, |_, _| {}).await;
    let followers: Vec<_> = (0..3).map(|_| subscribe(&d.socket).1).collect();
    for lines in &followers {
        lines.wait("the current state", |l| !l.all().is_empty()).await;
    }

    let id = d.dictate(400).await;
    d.idle().await;

    for lines in &followers {
        lines.wait("idle after the dictation", |l| states(&l.statuses()).len() >= 4).await;
        let statuses = lines.statuses();
        assert_eq!(states(&statuses), ["idle", "recording", "finishing", "idle"]);
        assert!(statuses.iter().any(|s| s.recording.as_ref().is_some_and(|r| r.session_id == id)));
        assert!(statuses.iter().any(|s| s.finishing.iter().any(|f| f.session_id == id)));
        let last = statuses.last().unwrap();
        assert!(last.retained.is_empty() && last.last_timings.is_some(), "{last:?}");
        assert!(statuses.windows(2).all(|w| w[0] != w[1]), "a line only when something changed");
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bar_shows_an_offline_recording_the_error_and_then_the_saved_count() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, deps| {
        deps.timing.stop_window = Duration::from_millis(800);
        deps.timing.notice_shown = Duration::from_millis(1500);
    })
    .await;
    let bar = Cli::start(&d.socket, Format::Waybar);
    let connected = |b: &Value| {
        b["tooltip"].as_str().is_some_and(|t| t.starts_with("psst: idle\nprofile: default\nserver: connected to "))
    };
    bar.lines.wait("idle and connected", |l| l.bar().last().is_some_and(connected)).await;
    server.set_listening(false);

    d.dictate(300).await;
    d.idle().await;
    bar.lines
        .wait("idle with the saved recording", |l| {
            l.bar().last().is_some_and(|b| b["alt"] == "idle" && b["text"] == "1")
        })
        .await;

    let lines = bar.lines.bar();
    let alts = alts(&lines);
    // Until the connection check fails, a new recording looks online.
    let shown = ["idle", "offline", "finishing", "error", "idle"];
    let shown_late = ["idle", "recording", "offline", "finishing", "error", "idle"];
    assert!(alts == shown || alts == shown_late, "{lines:#?}");
    let offline = lines.iter().find(|l| l["alt"] == "offline").unwrap();
    assert_eq!(offline["class"], serde_json::json!(["recording", "offline"]));
    let error = lines.iter().find(|l| l["alt"] == "error").unwrap();
    assert!(error["tooltip"].as_str().unwrap().contains("saved to History"), "{error}");
    let last = lines.last().unwrap();
    assert_eq!(last["class"], serde_json::json!(["idle", "saved"]));
    assert!(last["tooltip"].as_str().unwrap().contains("1 saved recording:"), "{last}");
    bar.close().unwrap();
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_levels_never_reach_followers() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("levels ".into()));
    let d = daemon(&server, |_, _| {}).await;
    let (_stream, lines) = subscribe(&d.socket);
    lines.wait("the current state", |l| !l.all().is_empty()).await;

    // A second of recording: about 50 frames, each moving the overlay's level meter.
    d.dictate(1_000).await;
    d.idle().await;
    lines.wait("idle after the dictation", |l| states(&l.statuses()).len() >= 4).await;

    let recording = lines.statuses().iter().filter(|s| matches!(s.state.as_str(), "starting" | "recording")).count();
    assert!(recording <= 3, "{:#?}", lines.all());
    assert!(d.wayland.overlays().iter().filter(|o| matches!(o, Some(OverlayState::Recording { .. }))).count() > 5);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_closer_than_the_follow_interval_arrive_as_one_line() {
    let server = FakeServer::start(FakeServerConfig::default());
    let d = daemon(&server, |_, deps| deps.timing.follow_interval = Duration::from_secs(1)).await;
    let saved: Vec<_> = (0..10).map(|_| save(&d.store, RecState::Complete, None)).collect();
    let (_stream, lines) = subscribe(&d.socket);
    lines.wait("the saved recordings", |l| l.statuses().last().is_some_and(|s| s.retained.len() == 10)).await;

    // Ten changes, each given time to reach the follower on its own.
    for id in saved {
        assert!(d.run(Request::Discard { session: id }).await.ok);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    lines.wait("none saved", |l| l.statuses().last().is_some_and(|s| s.retained.is_empty())).await;

    // The first change goes out at once and the rest a second later, together; a slow machine
    // stretching the burst past a second adds a line or two.
    let burst = lines.all().len() - 1;
    assert!((1..=4).contains(&burst), "{burst} lines for 10 changes");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_that_stops_reading_holds_up_neither_the_daemon_nor_other_followers() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("unblocked ".into()));
    let d = daemon(&server, |_, _| {}).await;
    // Long saved texts make each status line about 1 MB, more than a socket buffers, so the
    // follower that doesn't read stalls its connection on the first line.
    for _ in 0..40 {
        save(&d.store, RecState::Transcribed, Some("word ".repeat(5_000)));
    }
    let (mut stuck, _) = {
        let mut stream = UnixStream::connect(&d.socket).unwrap();
        stream.write_all(b"{\"cmd\":\"follow\"}\n").unwrap();
        (stream, ())
    };
    let (_stream, lines) = subscribe(&d.socket);
    lines.wait("the current state", |l| !l.all().is_empty()).await;

    d.dictate(300).await;
    d.idle().await;
    lines.wait("idle after the dictation", |l| states(&l.statuses()).len() >= 4).await;
    assert_eq!(states(&lines.statuses()), ["idle", "recording", "finishing", "idle"]);

    let latest = lines.statuses().last().unwrap().clone();
    let caught_up = tokio::task::spawn_blocking(move || {
        stuck.set_read_timeout(Some(WAIT)).unwrap();
        let mut reader = BufReader::new(&mut stuck);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "the daemon hung up");
            let status: Status = serde_json::from_str(&line).unwrap();
            if status == latest {
                return true;
            }
        }
    });
    assert!(caught_up.await.unwrap());
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_shows_the_daemon_stopped_and_follows_it_again_after_a_restart() {
    let server = FakeServer::start(FakeServerConfig::default());
    let dir = tempfile::tempdir().unwrap();
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    let cli = Cli::start(&d.socket, Format::Json);
    cli.lines.wait("idle", |l| l.statuses().last().is_some_and(|s| s.state == "idle")).await;

    d.stop().await;
    cli.lines.wait("stopped", |l| l.statuses().last().is_some_and(|s| s.state == "stopped")).await;
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    cli.lines.wait("idle again", |l| states(&l.statuses()).len() == 3).await;

    let statuses = cli.lines.statuses();
    assert_eq!(states(&statuses), ["idle", "stopped", "idle"]);
    assert_eq!(statuses.iter().filter(|s| s.state == "stopped").count(), 1, "retries print nothing new");
    assert_eq!(statuses.last().unwrap().connection, format!("connected to {}", server.endpoint()));
    cli.close().unwrap();
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bar_shows_a_recording_started_while_the_last_dictation_is_being_pasted() {
    // A niri that never answers holds the paste on its window lookup for a while.
    let dir = tempfile::tempdir().unwrap();
    let niri = dir.path().join("niri.sock");
    let _niri = std::os::unix::net::UnixListener::bind(&niri).unwrap();
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("first ".into()));
    let d = daemon(&server, |_, deps| {
        deps.niri_socket = Some(niri.clone());
        deps.timing.niri_lookup = Duration::from_secs(5);
    })
    .await;
    let bar = Cli::start(&d.socket, Format::Waybar);
    d.dictate(200).await;
    wait_until_async(|| async { d.status().await.state == "inserting" }).await;

    assert!(d.toggle().await.ok);
    bar.lines.wait("the new recording", |l| l.bar().last().is_some_and(|b| b["alt"] == "recording")).await;
    assert_eq!(d.status().await.state, "inserting", "shown while the paste is still pending");
    d.stop().await;
}

async fn wait_until_async<F: std::future::Future<Output = bool>>(check: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
