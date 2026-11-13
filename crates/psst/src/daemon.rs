//! The daemon: one task that owns every dictation, the connection to the server
//! and the Wayland seam, fed by the command socket, the recorder threads, the sessions and the
//! Wayland thread.
//!
//! **Dictations.** At most one is recording. After `stop` it joins the pipeline, where results
//! are delivered strictly in order; a `toggle` meanwhile starts the next recording at once, and
//! its frames wait (in the daemon, then in the session's frame buffer) until its `start` can go
//! out after the previous session's terminal message.
//!
//! **The connection.** One [`Client`] at a time, used by one session at a time (the "slot").
//! There are no reconnect timers: the daemon connects at start-up, once after the connection
//! closes, once after resume, at `start` (within 700 ms, or 3 s just after resume; up to 3 s on a
//! slow link), and within the 10 s window after `stop` when a recording has to be uploaded.
//!
//! **Neither the network nor the server stops the mic.** A recording that can't stream (no
//! connection, a rejection, `ready` later than 1 s, a lost session, a full frame buffer) goes
//! offline and keeps recording to its file; at `stop` it's uploaded once with its own
//! `session_id` and `resume: true`. If that fails too, it stays on disk for `psst retry`.
//!
//! **Followers.** `psst status --follow` subscribes on the command socket. After every input the
//! daemon compares a cheap summary of its state with the last one it published, and only when it
//! differs builds the full [`Status`] (which reads the recordings directory) and publishes it on a
//! watch channel. Each follower's connection task writes the latest status, at most once per
//! [`Timing::follow_interval`], so bursts arrive as one line and a follower that stops reading
//! stalls only its own task.

use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use jiff::Timestamp;
use psst_client::{
    AudioFrame, Client, FRAME_BUFFER, FrameSender, Session, SessionEvent, StartOpts, TrySendError, upload_file,
};
use psst_config::{ClientConfig, PasteAction};
use psst_proto::{
    ClientTimings, Endpoint, ErrorCode, FRAME_MS, SessionId,
    ws::{FinalMsg, Mode, ProgressState},
};
use psst_wayland::{Chord, OverlayState, WaylandCmd, WaylandEvent};
use tokio::{
    net::UnixListener,
    sync::{mpsc, oneshot, watch},
    time::{Instant, sleep_until},
};
use tracing::{debug, info, warn};

use crate::{
    audio::{
        AudioSource,
        recorder::{self, RecEvent, Recorder},
    },
    ipc::{ActiveInfo, Request, Response, RetainedInfo, Status},
    niri,
    pair::Credentials,
    store::{self, Last, Meta, RecState, STORAGE_BUDGET, Store},
    ui::{Wayland, WaylandSink},
};

/// The daemon's time limits. [`Timing::default`] is production's; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// `ready` must arrive this soon after `start` goes out, or the recording goes offline.
    pub ready_timeout: Duration,
    /// After `stop`, how long the daemon tries to get a connection for the upload.
    pub stop_window: Duration,
    /// The connection check at `start`. On a slow link it leaves room for the pong wait and a
    /// reconnect after it (TCP, the WebSocket upgrade, `hello`/`welcome`, and a round trip to
    /// spare): the pong wait plus 4 round trips, up to 3 s.
    pub start_budget: Duration,
    /// The connection check at `start` within [`Timing::resume_window`] of resume.
    pub resume_budget: Duration,
    pub resume_window: Duration,
    /// The pong wait inside the check. On a slow link it's 3 round trips, up to 2 s.
    pub pong: Duration,
    /// Longest single connection attempt inside the stop window.
    pub attempt_budget: Duration,
    /// First pause between attempts inside the stop window; it doubles up to 2 s.
    pub first_backoff: Duration,
    /// How long `Done` and `NothingHeard` stay on screen.
    pub done_shown: Duration,
    /// How long errors and flashes stay on screen.
    pub notice_shown: Duration,
    /// Longest the focused-window lookup over niri's IPC may take before the paste falls back to
    /// Ctrl+V.
    pub niri_lookup: Duration,
    /// Least time between two status lines to a follower (`psst status --follow`); changes in
    /// between are coalesced into the next line.
    pub follow_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ready_timeout: Duration::from_secs(1),
            stop_window: Duration::from_secs(10),
            start_budget: Duration::from_millis(700),
            resume_budget: Duration::from_secs(3),
            resume_window: Duration::from_secs(10),
            pong: Duration::from_millis(300),
            attempt_budget: Duration::from_secs(2),
            first_backoff: Duration::from_millis(250),
            done_shown: Duration::from_millis(800),
            notice_shown: Duration::from_secs(4),
            niri_lookup: niri::TIMEOUT,
            follow_interval: Duration::from_millis(100),
        }
    }
}

/// The longest the pong wait and the check at `start` grow to on a slow link, so a dead server is
/// still noticed quickly.
const MAX_PONG: Duration = Duration::from_secs(2);
const MAX_CHECK: Duration = Duration::from_secs(3);

/// What the daemon talks to besides the server, so tests can substitute each.
pub struct Deps {
    pub audio: Arc<dyn AudioSource>,
    pub wayland: Box<dyn Wayland>,
    /// Plays the error sound; called only when `sounds = true`.
    pub sound: Arc<dyn Fn() + Send + Sync>,
    /// niri's IPC socket, for the focused window.
    pub niri_socket: Option<PathBuf>,
    pub timing: Timing,
}

/// Where the daemon keeps its files.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_STATE_HOME/psst`.
    pub state_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/psst/daemon.sock`.
    pub socket: PathBuf,
}

/// A running daemon.
pub struct DaemonHandle {
    tx: mpsc::UnboundedSender<Input>,
    task: tokio::task::JoinHandle<()>,
    listener: tokio::task::JoinHandle<()>,
}

impl DaemonHandle {
    /// Handles one command, as the command socket does.
    pub async fn request(&self, request: Request) -> Response {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Request(request, tx)).is_err() {
            return Response::err("the daemon has stopped");
        }
        rx.await.unwrap_or_else(|_| Response::err("the daemon has stopped"))
    }

    /// Stops: a recording in progress is finished and kept for `psst retry`, followers are
    /// disconnected, and the command socket stops accepting connections.
    pub async fn shutdown(self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        let _ = self.task.await;
        self.listener.abort();
        let _ = self.listener.await;
    }
}

/// Starts the daemon: recovers the recordings, binds the command socket, connects to the server
/// once, and runs until [`DaemonHandle::shutdown`].
pub async fn spawn(cfg: ClientConfig, paths: Paths, deps: Deps) -> anyhow::Result<DaemonHandle> {
    // The socket first: it's what proves no other daemon is using the recordings.
    let listener = bind_socket(&paths.socket)?;
    let store = Store::open(&paths.state_dir)?;
    let recovery = store.recover(&cfg.profile)?;
    for id in &recovery.interrupted {
        info!("recording {id} was interrupted; kept as incomplete");
    }
    for id in &recovery.adopted {
        info!("adopted orphaned recording {id} as incomplete");
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (wayland_tx, wayland_rx) = mpsc::unbounded_channel();
    let listener = tokio::spawn(serve_socket(listener, tx.clone(), deps.timing.follow_interval));
    let (resumed_tx, mut resumed_rx) = mpsc::unbounded_channel();
    tokio::spawn(crate::logind::watch(resumed_tx));
    let resumed_input = tx.clone();
    tokio::spawn(async move {
        while resumed_rx.recv().await.is_some() {
            if resumed_input.send(Input::Resumed).is_err() {
                break;
            }
        }
    });
    let mut core = Core::new(cfg, store, deps, tx.clone(), wayland_tx, paths);
    core.connect(core.t.resume_budget, AttemptKind::Connect);
    let task = tokio::spawn(core.run(rx, wayland_rx));
    Ok(DaemonHandle { tx, task, listener })
}

fn bind_socket(path: &std::path::Path) -> anyhow::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!("another psst daemon is running ({})", path.display());
    }
    let _ = std::fs::remove_file(path);
    Ok(UnixListener::bind(path)?)
}

async fn serve_socket(listener: UnixListener, tx: mpsc::UnboundedSender<Input>, follow_interval: Duration) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let tx = tx.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut line = String::new();
            if read.read_line(&mut line).await.is_err() {
                return;
            }
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(Request::Follow) => {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    // A stopping daemon drops the reply, which closes the connection.
                    if tx.send(Input::Follow(reply_tx)).is_ok()
                        && let Ok(status) = reply_rx.await
                    {
                        serve_follower(status, read, write, follow_interval).await;
                    }
                    return;
                }
                Ok(request) => {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if tx.send(Input::Request(request, reply_tx)).is_err() {
                        return;
                    }
                    reply_rx.await.unwrap_or_else(|_| Response::err("the daemon is stopping"))
                }
                Err(err) => Response::err(format!("bad request: {err}")),
            };
            let mut out = serde_json::to_string(&response).expect("responses serialise");
            out.push('\n');
            let _ = write.write_all(out.as_bytes()).await;
        });
    }
}

/// One `follow` connection: the status now, then after each change, at most once per `interval`.
/// Only this task waits on a follower that stops reading; the watch channel keeps just the latest
/// status, so the daemon never does. It ends when the follower hangs up or the daemon stops.
async fn serve_follower(
    mut status: watch::Receiver<Status>,
    mut read: impl tokio::io::AsyncRead + Unpin,
    mut write: impl tokio::io::AsyncWrite + Unpin,
    interval: Duration,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = [0u8; 64];
    loop {
        let mut line = serde_json::to_string(&*status.borrow_and_update()).expect("statuses serialise");
        line.push('\n');
        if write.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        let earliest = Instant::now() + interval;
        loop {
            tokio::select! {
                changed = status.changed() => match changed {
                    Ok(()) => break,
                    Err(_) => return,
                },
                // The follower sends nothing after its request: this is it hanging up.
                read = read.read(&mut buf) => if matches!(read, Ok(0) | Err(_)) {
                    return;
                },
            }
        }
        sleep_until(earliest).await;
    }
}

enum Input {
    Request(Request, oneshot::Sender<Response>),
    Rec(SessionId, RecEvent),
    Session {
        id: SessionId,
        seq: u64,
        event: SessionEvent,
    },
    Conn {
        attempt: u64,
        result: Result<Client, String>,
        fresh: bool,
    },
    ConnClosed {
        epoch: u64,
        reason: String,
    },
    /// The focused window's app id for a paste.
    Focus {
        generation: u64,
        app_id: Option<String>,
    },
    Resumed,
    /// A follower subscribes to the status.
    Follow(oneshot::Sender<watch::Receiver<Status>>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Capturing, before the first frame.
    Starting,
    Recording,
    /// Stopped; the recorder is draining and flushing.
    Draining,
    /// `stop` went to the live session; waiting for the result.
    Waiting,
    /// Getting a connection for, or running, the resume upload.
    Uploading,
    /// Transcribed; waiting for its turn to be inserted.
    Ready,
}

/// How a recording streams.
#[derive(Debug, Clone, PartialEq)]
enum Net {
    /// Waiting for the connection check at `start`.
    Checking { deadline: Instant },
    /// Connected; waiting for the connection to be free to send `start`.
    Pending,
    /// Streaming live.
    Live { ready: bool, since: Instant },
    /// Recording to the file only; uploaded at `stop`.
    Offline(String),
}

struct SessionCtl {
    seq: u64,
    frames: FrameSender,
    ctl: mpsc::UnboundedSender<Ctl>,
}

enum Ctl {
    Stop(u64),
    Cancel,
}

struct Dictation {
    id: SessionId,
    profile: String,
    generation: u64,
    started_at: Timestamp,
    /// Started by `psst retry`; an incomplete recording is copied rather than pasted.
    copy_only: bool,
    phase: Phase,
    recorder: Option<Recorder>,
    stop_requested: bool,
    frames: u64,
    level: f32,
    max_recording_s: u32,
    samples: u64,
    gapped: bool,
    net: Net,
    session: Option<SessionCtl>,
    /// Frames captured while no session exists yet.
    backlog: Vec<AudioFrame>,
    uploads_left: u8,
    window_end: Option<Instant>,
    next_attempt: Instant,
    backoff: Duration,
    progress: Option<ProgressState>,
    result: Option<Box<FinalMsg>>,
    t_toggle: Option<Instant>,
    overlay_ms: Option<u32>,
    t_first_frame: Option<Instant>,
    t_stop: Option<Instant>,
    t_final: Option<Instant>,
}

impl Dictation {
    fn new(id: SessionId, profile: String, generation: u64, phase: Phase, net: Net) -> Dictation {
        Dictation {
            id,
            profile,
            generation,
            started_at: Timestamp::now(),
            copy_only: false,
            phase,
            recorder: None,
            stop_requested: false,
            frames: 0,
            level: 0.0,
            max_recording_s: 600,
            samples: 0,
            gapped: false,
            net,
            session: None,
            backlog: Vec::new(),
            uploads_left: 1,
            window_end: None,
            next_attempt: Instant::now(),
            backoff: Duration::ZERO,
            progress: None,
            result: None,
            t_toggle: None,
            overlay_ms: None,
            t_first_frame: None,
            t_stop: None,
            t_final: None,
        }
    }

    fn meta(&self, state: RecState) -> Meta {
        Meta {
            profile: self.profile.clone(),
            started_at: self.started_at,
            samples: self.samples,
            gapped: self.gapped,
            state,
            final_text: None,
            finalized_at: None,
        }
    }

    fn phase_text(&self) -> String {
        match (&self.phase, &self.net) {
            (Phase::Starting, _) => "starting".into(),
            (Phase::Recording, Net::Offline(reason)) => format!("recording, offline: {reason}"),
            (Phase::Recording, _) => "recording".into(),
            (Phase::Draining, _) => "stopping".into(),
            (Phase::Waiting, _) => "transcribing".into(),
            (Phase::Uploading, _) if self.session.is_some() => "uploading".into(),
            (Phase::Uploading, _) => "waiting for the server".into(),
            (Phase::Ready, _) => "waiting to be inserted".into(),
        }
    }
}

/// Text inserted again by `paste-last` or `copy-last`.
struct Redelivery {
    generation: u64,
    text: String,
    copy: bool,
    /// A retained `transcribed` recording to delete once delivered.
    delete: Option<SessionId>,
}

enum Delivering {
    Dictation(Box<Dictation>),
    Redelivery(Redelivery),
}

struct Inserting {
    generation: u64,
    what: Delivering,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    /// Ping the current connection, and reconnect if it doesn't answer.
    Check,
    Connect,
}

struct Core {
    cfg: ClientConfig,
    store: Store,
    deps: Deps,
    t: Timing,
    tx: mpsc::UnboundedSender<Input>,
    wayland_tx: mpsc::UnboundedSender<WaylandEvent>,
    credentials: PathBuf,

    client: Option<Client>,
    epoch: u64,
    connected_at: Instant,
    conn_error: Option<String>,
    /// [`Client::round_trip`] of the latest connection, kept while there is none.
    round_trip: Duration,
    attempt: Option<u64>,
    attempts: u64,
    /// The session that holds the connection.
    slot: Option<SessionId>,
    resumed_at: Option<Instant>,
    max_recording_s: u32,

    recording: Option<Dictation>,
    pipeline: VecDeque<Dictation>,
    redeliver: VecDeque<Redelivery>,
    inserting: Option<Inserting>,
    generation: u64,
    seq: u64,

    sink: Option<Box<dyn WaylandSink>>,
    overlay: Option<Option<OverlayState>>,
    overlay_at: Instant,
    notice: Option<(OverlayState, Instant)>,
    awaiting_shown: Option<SessionId>,
    last_timings: Option<ClientTimings>,
    stopping: Option<(oneshot::Sender<()>, Instant)>,

    /// The status followers see.
    follow: watch::Sender<Status>,
    /// What the published status was built from: [`Core::status_summary`] and the store's
    /// revision.
    published: Option<(Status, u64)>,
}

impl Core {
    fn new(
        cfg: ClientConfig,
        store: Store,
        deps: Deps,
        tx: mpsc::UnboundedSender<Input>,
        wayland_tx: mpsc::UnboundedSender<WaylandEvent>,
        paths: Paths,
    ) -> Core {
        let t = deps.timing.clone();
        Core {
            cfg,
            store,
            deps,
            t,
            tx,
            wayland_tx,
            credentials: paths.state_dir.join("credentials"),
            client: None,
            epoch: 0,
            connected_at: Instant::now(),
            conn_error: None,
            round_trip: Duration::ZERO,
            attempt: None,
            attempts: 0,
            slot: None,
            resumed_at: None,
            max_recording_s: 600,
            recording: None,
            pipeline: VecDeque::new(),
            redeliver: VecDeque::new(),
            inserting: None,
            generation: 0,
            seq: 0,
            sink: None,
            overlay: None,
            overlay_at: Instant::now(),
            notice: None,
            awaiting_shown: None,
            last_timings: None,
            stopping: None,
            follow: watch::Sender::new(Status::stopped()),
            published: None,
        }
    }

    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Input>,
        mut wayland_rx: mpsc::UnboundedReceiver<WaylandEvent>,
    ) {
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                input = rx.recv() => match input {
                    Some(input) => self.handle(input),
                    None => break,
                },
                Some(event) = wayland_rx.recv() => self.on_wayland(event),
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => self.on_tick(),
            }
            if let Some((_, until)) = &self.stopping {
                let draining = self.pipeline.iter().any(|d| d.phase == Phase::Draining);
                if (self.recording.is_none() && !draining) || Instant::now() >= *until {
                    break;
                }
            }
            self.pump();
            self.publish();
        }
        if let Some((reply, _)) = self.stopping.take() {
            let _ = reply.send(());
        }
        if let Some(client) = &self.client {
            client.close();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Request(request, reply) => {
                let response = self.on_request(request);
                let _ = reply.send(response);
            }
            Input::Rec(id, event) => self.on_rec(id, event),
            Input::Session { id, seq, event } => self.on_session(id, seq, event),
            Input::Conn { attempt, result, fresh } => self.on_conn(attempt, result, fresh),
            Input::ConnClosed { epoch, reason } => self.on_conn_closed(epoch, reason),
            Input::Focus { generation, app_id } => self.on_focus(generation, app_id),
            Input::Resumed => {
                self.resumed_at = Some(Instant::now());
                if let Some(client) = self.client.take() {
                    self.epoch += 1;
                    client.close();
                }
                self.connect(self.t.resume_budget, AttemptKind::Connect);
            }
            Input::Follow(reply) => {
                if self.stopping.is_none() {
                    let status = self.follow.subscribe();
                    self.publish();
                    let _ = reply.send(status);
                }
            }
            Input::Shutdown(reply) => {
                if let Some(rec) = &mut self.recording {
                    rec.stop_requested = true;
                    if let Some(recorder) = &rec.recorder {
                        recorder.stop();
                    }
                }
                self.stopping = Some((reply, Instant::now() + Duration::from_secs(2)));
            }
        }
    }

    // ---- commands ----

    fn on_request(&mut self, request: Request) -> Response {
        if self.stopping.is_some() {
            return Response::err("the daemon is stopping");
        }
        match request {
            Request::Toggle { profile } => match self.recording {
                Some(_) => self.stop_recording(),
                None => self.start_recording(profile),
            },
            Request::Start => match self.recording {
                Some(_) => {
                    self.flash("already recording");
                    Response::err("already recording")
                }
                None => self.start_recording(None),
            },
            Request::Stop => match self.recording {
                Some(_) => self.stop_recording(),
                None => Response::err("not recording"),
            },
            Request::Cancel => self.cancel(),
            Request::Retry { session } => self.retry(session),
            Request::PasteLast { session } => self.redeliver(session, false),
            Request::CopyLast { session } => self.redeliver(session, true),
            Request::Discard { session } => self.discard(session),
            Request::Status => Response { status: Some(self.status()), ..Response::ok("ok") },
            Request::Follow => Response::err("follow works only over the command socket"),
        }
    }

    fn start_recording(&mut self, profile: Option<String>) -> Response {
        let usage = self.store.usage();
        if usage > STORAGE_BUDGET {
            let message = format!("saved recordings use {} MB: see psst status", usage / 1_000_000);
            self.error_notice(&message);
            return Response::err(message);
        }
        let id = SessionId::new_v4();
        let generation = self.next_generation();
        let profile = profile.unwrap_or_else(|| self.cfg.profile.clone());
        let mut d = Dictation::new(id, profile, generation, Phase::Starting, Net::Pending);
        d.t_toggle = Some(Instant::now());
        d.max_recording_s = self.max_recording_s;
        if let Err(err) = self.store.write_meta(id, &d.meta(RecState::Recording)) {
            let message = format!("cannot save the recording: {err}");
            self.error_notice(&message);
            return Response::err(message);
        }
        let tx = self.tx.clone();
        match recorder::start(self.deps.audio.clone(), self.store.wav_path(id), move |event| {
            let _ = tx.send(Input::Rec(id, event));
        }) {
            Ok(recorder) => d.recorder = Some(recorder),
            Err(err) => {
                let _ = self.store.delete(id);
                let message = format!("cannot start recording: {err}");
                self.error_notice(&message);
                return Response::err(message);
            }
        }
        d.net = self.begin_check();
        self.recording = Some(d);
        // `OverlayShown` comes only when the overlay appears. One already on screen (a notice, or
        // the previous dictation finishing) just changes state, and the time isn't measured.
        self.awaiting_shown = (!self.overlay_visible()).then_some(id);
        info!("recording {id}");
        Response::ok(format!("recording {id}"))
    }

    fn stop_recording(&mut self) -> Response {
        let Some(rec) = &mut self.recording else { return Response::err("not recording") };
        if rec.phase == Phase::Starting {
            rec.stop_requested = true;
            return Response::ok("stopping as soon as the recording runs");
        }
        let mut d = self.recording.take().expect("checked");
        d.phase = Phase::Draining;
        d.t_stop = Some(Instant::now());
        if let Some(recorder) = &d.recorder {
            recorder.stop();
        }
        let (id, generation) = (d.id, d.generation);
        self.pipeline.push_back(d);
        self.wayland(WaylandCmd::SnapshotClipboard { generation });
        info!("stopped {id}");
        Response::ok(format!("stopped {id}"))
    }

    fn cancel(&mut self) -> Response {
        let d = match self.recording.take() {
            Some(d) => d,
            None => match self.pipeline.pop_back() {
                Some(d) => d,
                None => return Response::err("nothing to cancel"),
            },
        };
        let id = d.id;
        self.drop_dictation(d);
        info!("cancelled {id}");
        Response::ok(format!("cancelled {id}"))
    }

    /// Ends a dictation for good: stops its capture and session and deletes its files.
    fn drop_dictation(&mut self, d: Dictation) {
        if let Some(recorder) = &d.recorder {
            recorder.abort();
        }
        if let Some(session) = &d.session {
            let _ = session.ctl.send(Ctl::Cancel);
        }
        if let Err(err) = self.store.delete(d.id) {
            warn!("cannot delete recording {}: {err}", d.id);
        }
    }

    fn in_flight(&self, id: SessionId) -> bool {
        self.recording.as_ref().is_some_and(|d| d.id == id)
            || self.pipeline.iter().any(|d| d.id == id)
            || matches!(&self.inserting, Some(Inserting { what: Delivering::Dictation(d), .. }) if d.id == id)
    }

    fn retry(&mut self, session: Option<SessionId>) -> Response {
        let retained = self.store.list();
        let target =
            match session {
                Some(id) => match retained.iter().find(|r| r.id == id) {
                    Some(rec) => rec,
                    None => return Response::err(format!("no saved recording {id}")),
                },
                None => match retained.iter().rev().find(|r| {
                    matches!(r.meta.state, RecState::Complete | RecState::Incomplete) && !self.in_flight(r.id)
                }) {
                    Some(rec) => rec,
                    None => return Response::err("no saved recording to retry"),
                },
            };
        let id = target.id;
        if self.in_flight(id) {
            return Response::err(format!("{id} is still in progress"));
        }
        match target.meta.state {
            RecState::Transcribed => {
                return Response::err(format!("{id} is already transcribed: run psst paste-last --session {id}"));
            }
            RecState::Recording => return Response::err(format!("{id} is still recording")),
            RecState::Complete | RecState::Incomplete => {}
        }
        if target.wav_bytes == 0 {
            return Response::err(format!("{id} has no audio: run psst discard --session {id}"));
        }
        let generation = self.next_generation();
        let mut d = Dictation::new(id, target.meta.profile.clone(), generation, Phase::Uploading, Net::Pending);
        d.started_at = target.meta.started_at;
        d.samples = target.meta.samples;
        d.gapped = target.meta.gapped;
        d.copy_only = target.meta.state == RecState::Incomplete;
        d.t_stop = Some(Instant::now());
        self.begin_upload(&mut d);
        self.pipeline.push_back(d);
        self.wayland(WaylandCmd::SnapshotClipboard { generation });
        info!("retrying {id}");
        Response::ok(format!("retrying {id}"))
    }

    fn redeliver(&mut self, session: Option<SessionId>, copy: bool) -> Response {
        let retained = self.store.list();
        let (text, delete) = match session {
            Some(id) => {
                if self.in_flight(id) {
                    return Response::err(format!("{id} is still in progress"));
                }
                match retained.iter().find(|r| r.id == id) {
                    Some(rec) if rec.meta.state == RecState::Transcribed => {
                        (rec.meta.final_text.clone().unwrap_or_default(), Some(id))
                    }
                    Some(_) => {
                        return Response::err(format!("{id} isn't transcribed yet: run psst retry --session {id}"));
                    }
                    None => return Response::err(format!("no saved recording {id}")),
                }
            }
            None => match self.store.read_last() {
                Some(last) => {
                    let retained_too =
                        retained.iter().any(|r| r.id == last.session_id && r.meta.state == RecState::Transcribed)
                            && !self.in_flight(last.session_id);
                    (last.text, retained_too.then_some(last.session_id))
                }
                None => return Response::err("nothing dictated yet"),
            },
        };
        let generation = self.next_generation();
        self.wayland(WaylandCmd::SnapshotClipboard { generation });
        self.redeliver.push_back(Redelivery { generation, text, copy, delete });
        Response::ok(if copy { "copying" } else { "pasting" })
    }

    fn discard(&mut self, id: SessionId) -> Response {
        if self.in_flight(id) {
            return Response::err(format!("{id} is in progress: use psst cancel"));
        }
        if !self.store.meta_path(id).exists() && !self.store.wav_path(id).exists() {
            return Response::err(format!("no saved recording {id}"));
        }
        match self.store.delete(id) {
            Ok(()) => Response::ok(format!("discarded {id}")),
            Err(err) => Response::err(format!("cannot delete {id}: {err}")),
        }
    }

    fn status(&self) -> Status {
        self.complete_status(self.status_summary())
    }

    /// The status without the saved recordings and their warnings, which come from the disk.
    fn status_summary(&self) -> Status {
        let state = if self.inserting.is_some() {
            "inserting"
        } else if let Some(rec) = &self.recording {
            if rec.phase == Phase::Starting { "starting" } else { "recording" }
        } else if !self.pipeline.is_empty() {
            "finishing"
        } else {
            "idle"
        };
        let connection = match (&self.client, self.attempt, &self.conn_error) {
            (Some(client), _, _) if !client.is_closed() => format!("connected to {}", client.endpoint()),
            (_, Some(_), _) => "connecting".into(),
            (_, _, Some(err)) => format!("offline: {err}"),
            _ => "offline".into(),
        };
        let info = |d: &Dictation| ActiveInfo {
            session_id: d.id,
            profile: d.profile.clone(),
            phase: d.phase_text(),
            offline: match &d.net {
                Net::Offline(reason) => Some(reason.clone()),
                _ => None,
            },
        };
        let now = Instant::now();
        let error = match &self.notice {
            Some((OverlayState::Error { message }, until)) if *until > now => Some(message.clone()),
            _ => None,
        };
        Status {
            state: state.into(),
            connection,
            error,
            recording: self.recording.as_ref().map(info),
            finishing: self.pipeline.iter().map(info).collect(),
            retained: Vec::new(),
            warnings: Vec::new(),
            last_timings: self.last_timings,
        }
    }

    /// `summary` with the saved recordings and their warnings.
    fn complete_status(&self, mut summary: Status) -> Status {
        let retained = self.store.list();
        summary.warnings = self.store.warnings(&retained);
        summary.retained = retained
            .into_iter()
            .map(|r| {
                let in_progress = self.in_flight(r.id);
                let hint = match r.meta.state {
                    RecState::Transcribed => format!("psst paste-last --session {}", r.id),
                    RecState::Recording if in_progress => "recording now".into(),
                    _ if in_progress => "in progress".into(),
                    _ => format!("psst retry --session {}", r.id),
                };
                RetainedInfo {
                    session_id: r.id,
                    state: serde_json::to_value(r.meta.state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    profile: r.meta.profile,
                    started_at: r.meta.started_at,
                    duration_ms: r.meta.samples / 16,
                    gapped: r.meta.gapped,
                    text: r.meta.final_text,
                    hint,
                    in_progress,
                }
            })
            .collect();
        summary
    }

    /// Publishes the status to followers if it changed. The summary is cheap, so this runs after
    /// every input (audio frames included); the recordings directory is read only when the
    /// summary or the store changed.
    fn publish(&mut self) {
        if self.follow.receiver_count() == 0 {
            self.published = None;
            return;
        }
        let key = (self.status_summary(), self.store.revision());
        if self.published.as_ref() == Some(&key) {
            return;
        }
        let status = self.complete_status(key.0.clone());
        self.published = Some(key);
        self.follow.send_if_modified(|current| {
            let changed = *current != status;
            if changed {
                *current = status;
            }
            changed
        });
    }

    // ---- the connection ----

    fn endpoint_is_tcp(&self) -> bool {
        matches!(self.cfg.server, Endpoint::Tcp { .. })
    }

    /// The network state of a recording that starts now.
    fn begin_check(&mut self) -> Net {
        let resumed = self.resumed_at.is_some_and(|at| at.elapsed() < self.t.resume_window);
        let budget = self.check_budget(if resumed { self.t.resume_budget } else { self.t.start_budget });
        let deadline = Instant::now() + budget;
        let alive = self.client.as_ref().is_some_and(|c| !c.is_closed());
        if alive && !self.endpoint_is_tcp() {
            return Net::Pending;
        }
        if self.attempt.is_none() {
            self.connect(budget, if alive { AttemptKind::Check } else { AttemptKind::Connect });
        }
        Net::Checking { deadline }
    }

    /// The pong wait at `start`, scaled to the server's round trip ([`Timing::pong`]).
    fn pong_budget(&self) -> Duration {
        self.t.pong.max(self.round_trip.saturating_mul(3).min(MAX_PONG))
    }

    /// The connection check at `start`, scaled to the server's round trip and at least `floor`
    /// ([`Timing::start_budget`]).
    fn check_budget(&self, floor: Duration) -> Duration {
        floor.max((self.pong_budget() + self.round_trip.saturating_mul(4)).min(MAX_CHECK))
    }

    /// Starts one connection attempt (or a check of the current connection) within `budget`.
    fn connect(&mut self, budget: Duration, kind: AttemptKind) {
        if self.attempt.is_some() {
            return;
        }
        self.attempts += 1;
        let attempt = self.attempts;
        self.attempt = Some(attempt);
        let endpoint = self.cfg.server.clone();
        let token = if self.endpoint_is_tcp() {
            match Credentials::load(&self.credentials) {
                Ok(Some(creds)) => Some(creds.token),
                Ok(None) => None,
                Err(err) => {
                    warn!("cannot read {}: {err}", self.credentials.display());
                    None
                }
            }
        } else {
            None
        };
        let current = if kind == AttemptKind::Check { self.client.clone() } else { None };
        let pong = self.pong_budget().min(budget);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let work = async {
                if let Some(mut client) = current {
                    match client.check_alive(pong).await {
                        Ok(()) => return (Ok(client), false),
                        Err(err) => {
                            debug!("connection check failed: {err}");
                            client.close();
                        }
                    }
                }
                let result = Client::connect(&endpoint, token.as_ref()).await.map_err(|e| e.to_string());
                (result, true)
            };
            let (result, fresh) = match tokio::time::timeout(budget, work).await {
                Ok(done) => done,
                Err(_) => (Err(format!("no answer from {endpoint} within {} ms", budget.as_millis())), true),
            };
            let _ = tx.send(Input::Conn { attempt, result, fresh });
        });
    }

    fn on_conn(&mut self, attempt: u64, result: Result<Client, String>, fresh: bool) {
        if self.attempt == Some(attempt) {
            self.attempt = None;
        }
        match result {
            Ok(client) => {
                self.round_trip = client.round_trip();
                if fresh {
                    if let Some(old) = self.client.take() {
                        old.close();
                    }
                    self.epoch += 1;
                    let epoch = self.epoch;
                    self.max_recording_s = client.welcome().limits.max_recording_s;
                    let watcher = client.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let reason = watcher.closed().await;
                        let _ = tx.send(Input::ConnClosed { epoch, reason });
                    });
                    info!(
                        "connected to {} as {} ({} ms round trip)",
                        client.endpoint(),
                        client.welcome().device,
                        self.round_trip.as_millis()
                    );
                    self.client = Some(client);
                    self.connected_at = Instant::now();
                    // A session on the old connection is lost with it.
                    self.slot = None;
                }
                self.conn_error = None;
                if let Some(rec) = &mut self.recording
                    && matches!(rec.net, Net::Checking { .. })
                {
                    rec.net = Net::Pending;
                }
            }
            Err(reason) => {
                info!("cannot reach the server: {reason}");
                if fresh && let Some(old) = self.client.take() {
                    old.close();
                }
                self.conn_error = Some(reason.clone());
                if let Some(rec) = &self.recording
                    && matches!(rec.net, Net::Checking { .. })
                {
                    let id = rec.id;
                    self.went_offline(id, reason);
                }
            }
        }
    }

    fn on_conn_closed(&mut self, epoch: u64, reason: String) {
        if epoch != self.epoch {
            return;
        }
        info!("connection closed: {reason}");
        let lived = self.connected_at.elapsed();
        self.client = None;
        self.slot = None;
        self.conn_error = Some(reason.clone());
        if let Some(rec) = &self.recording
            && rec.net == Net::Pending
        {
            let id = rec.id;
            self.went_offline(id, reason);
        }
        // Reconnect once, unless the connection died right after it was made.
        if lived >= Duration::from_secs(2) && self.stopping.is_none() {
            self.connect(self.check_budget(self.t.start_budget).max(Duration::from_secs(1)), AttemptKind::Connect);
        }
    }

    fn usable_client(&mut self) -> Option<Client> {
        if self.client.as_ref().is_some_and(Client::is_closed) {
            self.client = None;
            self.slot = None;
        }
        self.client.clone()
    }

    fn spawn_session(&mut self, id: SessionId, mut session: Session) -> SessionCtl {
        self.seq += 1;
        let seq = self.seq;
        let frames = session.frames();
        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut ctl_open = true;
            loop {
                tokio::select! {
                    event = session.next_event() => {
                        let terminal = event.is_terminal();
                        let _ = tx.send(Input::Session { id, seq, event });
                        if terminal {
                            break;
                        }
                    }
                    ctl = ctl_rx.recv(), if ctl_open => match ctl {
                        Some(Ctl::Stop(samples)) => {
                            if let Err(err) = session.stop(samples).await {
                                debug!("stop for {id}: {err}");
                            }
                        }
                        Some(Ctl::Cancel) => {
                            let _ = session.cancel().await;
                        }
                        None => ctl_open = false,
                    },
                }
            }
        });
        SessionCtl { seq, frames, ctl: ctl_tx }
    }

    fn start_live(&mut self, client: &mut Client) {
        let Some(rec) = &mut self.recording else { return };
        let (id, profile) = (rec.id, rec.profile.clone());
        let session = client.start(StartOpts {
            session_id: id,
            profile: Some(profile),
            mode: Mode::Live,
            resume: false,
            eval: None,
        });
        let ctl = self.spawn_session(id, session);
        let rec = self.recording.as_mut().expect("checked");
        rec.max_recording_s = client.welcome().limits.max_recording_s;
        rec.net = Net::Live { ready: false, since: Instant::now() };
        // Frames captured so far go first; the session's buffer holds them until `ready`.
        let backlog = std::mem::take(&mut rec.backlog);
        let full = backlog.into_iter().any(|frame| ctl.frames.try_send(frame).is_err());
        rec.session = Some(ctl);
        self.slot = Some(id);
        if full {
            self.went_offline(id, "too much audio waiting for the server".into());
        }
    }

    fn start_upload(&mut self, client: &mut Client, index: usize) {
        let d = &self.pipeline[index];
        let (id, profile) = (d.id, d.profile.clone());
        let wav = self.store.wav_path(id);
        let opts = StartOpts { session_id: id, profile: Some(profile), mode: Mode::Upload, resume: true, eval: None };
        let session = upload_file(client, opts, &wav);
        let ctl = self.spawn_session(id, session);
        let d = &mut self.pipeline[index];
        d.session = Some(ctl);
        d.progress = None;
        self.slot = Some(id);
        info!("uploading {id}");
    }

    /// Moves a stopped dictation to the resume upload, with a fresh 10 s connection window.
    fn begin_upload(&self, d: &mut Dictation) {
        d.uploads_left = d.uploads_left.saturating_sub(1);
        d.phase = Phase::Uploading;
        d.session = None;
        d.backlog = Vec::new();
        d.progress = None;
        d.window_end = Some(Instant::now() + self.t.stop_window);
        d.next_attempt = Instant::now();
        d.backoff = self.t.first_backoff;
    }

    /// A recording can no longer stream; it keeps recording to its file.
    fn went_offline(&mut self, id: SessionId, reason: String) {
        let Some(d) = self.dictation_mut(id) else { return };
        if matches!(d.net, Net::Offline(_)) {
            return;
        }
        info!("{id} is offline: {reason}");
        d.net = Net::Offline(reason);
        d.backlog = Vec::new();
        let recording = matches!(d.phase, Phase::Starting | Phase::Recording);
        if recording {
            self.sound();
        }
    }

    // ---- recorder events ----

    fn dictation_mut(&mut self, id: SessionId) -> Option<&mut Dictation> {
        if let Some(rec) = &mut self.recording
            && rec.id == id
        {
            return Some(rec);
        }
        self.pipeline.iter_mut().find(|d| d.id == id)
    }

    fn on_rec(&mut self, id: SessionId, event: RecEvent) {
        match event {
            RecEvent::Frame { frame, level } => self.on_frame(id, *frame, level),
            RecEvent::Gap => {
                if let Some(d) = self.dictation_mut(id) {
                    d.gapped = true;
                }
                self.flash("audio glitch: filled with silence");
            }
            RecEvent::Failed { reason, samples } => {
                let Some(mut d) = self.take_dictation(id) else { return };
                warn!("recording {id} failed: {reason}");
                d.samples = samples;
                if let Some(session) = d.session.take() {
                    let _ = session.ctl.send(Ctl::Cancel);
                }
                if samples == 0 {
                    let _ = self.store.delete(id);
                } else if let Err(err) = self.store.write_meta(id, &d.meta(RecState::Incomplete)) {
                    warn!("cannot save {id}: {err}");
                }
                self.error_notice(&format!("recording failed: {reason}"));
            }
            RecEvent::Finished { samples, gapped } => self.on_finished(id, samples, gapped),
        }
    }

    fn take_dictation(&mut self, id: SessionId) -> Option<Dictation> {
        if self.recording.as_ref().is_some_and(|d| d.id == id) {
            return self.recording.take();
        }
        let pos = self.pipeline.iter().position(|d| d.id == id)?;
        self.pipeline.remove(pos)
    }

    fn on_frame(&mut self, id: SessionId, frame: AudioFrame, level: f32) {
        let is_recording = self.recording.as_ref().is_some_and(|d| d.id == id);
        let Some(d) = self.dictation_mut(id) else { return };
        d.frames += 1;
        d.level = level;
        let first = d.t_first_frame.is_none();
        if first {
            d.t_first_frame = Some(Instant::now());
            if d.phase == Phase::Starting {
                d.phase = Phase::Recording;
            }
        }
        let mut lost = None;
        match (&d.net, &d.session) {
            (Net::Live { .. }, Some(session)) => match session.frames.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full) => lost = Some(("the server isn't keeping up".to_string(), true)),
                Err(TrySendError::Closed) => lost = Some(("the session ended".to_string(), false)),
            },
            (Net::Checking { .. } | Net::Pending, _) => {
                if d.backlog.len() < FRAME_BUFFER {
                    d.backlog.push(frame);
                } else {
                    lost = Some(("too much audio waiting for the server".to_string(), false));
                }
            }
            _ => {}
        }
        let over_limit = d.frames * FRAME_MS as u64 >= d.max_recording_s as u64 * 1000;
        let stop_now = is_recording && ((first && d.stop_requested) || over_limit);
        if let Some((reason, close)) = lost {
            if close && let Some(client) = &self.client {
                client.close();
            }
            self.went_offline(id, reason);
        }
        if stop_now {
            if over_limit {
                info!("{id} reached the recording limit");
            }
            self.stop_recording();
        }
    }

    fn on_finished(&mut self, id: SessionId, samples: u64, gapped: bool) {
        let Some(pos) = self.pipeline.iter().position(|d| d.id == id) else {
            // The daemon is stopping with this recording still running: keep it for retry.
            if let Some(mut d) = self.recording.take_if(|d| d.id == id) {
                d.samples = samples;
                d.gapped |= gapped;
                let _ = self.store.write_meta(id, &d.meta(RecState::Complete));
                store::sync_files(&[self.store.wav_path(id), self.store.meta_path(id)], self.store.recordings_dir());
            }
            return;
        };
        let d = &mut self.pipeline[pos];
        d.samples = samples;
        d.gapped |= gapped;
        d.recorder = None;
        if let Err(err) = self.store.write_meta(id, &d.meta(RecState::Complete)) {
            warn!("cannot save {id}: {err}");
        }
        // fsync off the critical path: `stop` goes out without waiting for the disk.
        let files = [self.store.wav_path(id), self.store.meta_path(id)];
        let dir = self.store.recordings_dir().to_path_buf();
        tokio::task::spawn_blocking(move || store::sync_files(&files, &dir));
        let d = &mut self.pipeline[pos];
        match (&d.net, &d.session) {
            (Net::Live { .. }, Some(session)) => {
                let _ = session.ctl.send(Ctl::Stop(samples));
                d.phase = Phase::Waiting;
            }
            _ => {
                let mut d = self.pipeline.remove(pos).expect("in range");
                if d.uploads_left > 0 {
                    self.begin_upload(&mut d);
                    self.pipeline.insert(pos, d);
                } else {
                    self.saved(d, "no connection to the server".into());
                }
            }
        }
    }

    // ---- session events ----

    fn on_session(&mut self, id: SessionId, seq: u64, event: SessionEvent) {
        let terminal = event.is_terminal();
        if terminal && self.slot == Some(id) {
            self.slot = None;
        }
        let Some(d) = self.dictation_mut(id) else { return };
        if d.session.as_ref().map(|s| s.seq) != Some(seq) {
            return;
        }
        if terminal {
            d.session = None;
        }
        let stopped = !matches!(d.phase, Phase::Starting | Phase::Recording | Phase::Draining);
        match event {
            SessionEvent::Ready { .. } => {
                if let Net::Live { ready, .. } = &mut d.net {
                    *ready = true;
                }
            }
            SessionEvent::Progress { state, .. } => d.progress = Some(state),
            SessionEvent::Final(msg) => self.on_final(id, msg),
            SessionEvent::Cancelled => {}
            SessionEvent::Rejected(err) if !stopped => self.went_offline(id, err.message),
            SessionEvent::Rejected(err) => self.after_failure(id, format!("the server refused it: {}", err.message)),
            SessionEvent::Error(err) if !stopped => self.went_offline(id, err.message),
            SessionEvent::Error(err) if err.code == ErrorCode::BadAudio => {
                self.after_failure(id, format!("the server got different audio: {}", err.message))
            }
            SessionEvent::Error(err) => {
                let d = self.take_dictation(id).expect("found above");
                self.saved(d, err.message);
            }
            SessionEvent::Lost(reason) if !stopped => self.went_offline(id, reason.to_string()),
            SessionEvent::Lost(reason) => self.after_failure(id, reason.to_string()),
        }
    }

    /// A stopped dictation's session failed: upload once more if it may, else keep it for retry.
    fn after_failure(&mut self, id: SessionId, reason: String) {
        let Some(pos) = self.pipeline.iter().position(|d| d.id == id) else { return };
        let mut d = self.pipeline.remove(pos).expect("in range");
        if d.uploads_left > 0 {
            info!("{id}: {reason}; uploading it");
            self.begin_upload(&mut d);
            self.pipeline.insert(pos, d);
        } else {
            self.saved(d, reason);
        }
    }

    /// Gives up on a dictation for now: its recording stays on disk for `psst retry`.
    fn saved(&mut self, d: Dictation, reason: String) {
        warn!("{} saved for retry: {reason}", d.id);
        if let Some(session) = &d.session {
            let _ = session.ctl.send(Ctl::Cancel);
        }
        self.error_notice(&format!("saved, psst retry ({reason})"));
    }

    fn on_final(&mut self, id: SessionId, msg: Box<FinalMsg>) {
        let Some(pos) = self.pipeline.iter().position(|d| d.id == id) else { return };
        let d = &mut self.pipeline[pos];
        d.t_final = Some(Instant::now());
        let text = msg.r#final.text.clone();
        d.result = Some(msg);
        if text.trim().is_empty() {
            let d = self.pipeline.remove(pos).expect("in range");
            info!("{id}: nothing heard");
            if let Err(err) = self.store.delete(id) {
                warn!("cannot delete {id}: {err}");
            }
            self.report(&d, None);
            self.notice(OverlayState::NothingHeard, self.t.done_shown * 2);
            return;
        }
        let finalized_at = Timestamp::now();
        let mut meta = d.meta(RecState::Transcribed);
        meta.final_text = Some(text.clone());
        meta.finalized_at = Some(finalized_at);
        d.phase = Phase::Ready;
        if let Err(err) = self.store.write_meta(id, &meta) {
            warn!("cannot save the text of {id}: {err}");
        }
        if let Err(err) = self.store.write_last(&Last { session_id: id, finalized_at, text }) {
            warn!("cannot write last.json: {err}");
        }
    }

    // ---- delivery ----

    fn deliver_next(&mut self) {
        if self.inserting.is_some() {
            return;
        }
        if let Some(r) = self.redeliver.pop_front() {
            let (generation, copy) = (r.generation, r.copy);
            self.inserting = Some(Inserting { generation, what: Delivering::Redelivery(r) });
            self.begin_insert(generation, copy);
            return;
        }
        if let Some(d) = self.pipeline.pop_front_if(|d| d.phase == Phase::Ready) {
            let (generation, copy) = (d.generation, d.copy_only);
            self.inserting = Some(Inserting { generation, what: Delivering::Dictation(Box::new(d)) });
            self.begin_insert(generation, copy);
        }
    }

    fn begin_insert(&mut self, generation: u64, copy: bool) {
        if copy {
            self.send_insert(generation, PasteAction::CopyOnly, 0);
            return;
        }
        match self.deps.niri_socket.clone() {
            Some(socket) => {
                let tx = self.tx.clone();
                let timeout = self.deps.timing.niri_lookup;
                tokio::spawn(async move {
                    let app_id = niri::focused_app_id(&socket, timeout).await;
                    let _ = tx.send(Input::Focus { generation, app_id });
                });
            }
            None => self.on_focus(generation, None),
        }
    }

    fn on_focus(&mut self, generation: u64, app_id: Option<String>) {
        let rule = self.cfg.paste_for(app_id.as_deref());
        debug!("pasting into {app_id:?} with {rule:?}");
        self.send_insert(generation, rule.action, rule.delay_ms);
    }

    fn send_insert(&mut self, generation: u64, action: PasteAction, delay_ms: u32) {
        let Some(inserting) = &self.inserting else { return };
        if inserting.generation != generation {
            return;
        }
        let text = match &inserting.what {
            Delivering::Dictation(d) => d.result.as_ref().map(|r| r.r#final.text.clone()).unwrap_or_default(),
            Delivering::Redelivery(r) => r.text.clone(),
        };
        let cmd = match action {
            PasteAction::CtrlV => WaylandCmd::Paste { generation, text, chord: Chord::CtrlV, delay_ms },
            PasteAction::CtrlShiftV => WaylandCmd::Paste { generation, text, chord: Chord::CtrlShiftV, delay_ms },
            PasteAction::CopyOnly => WaylandCmd::Copy { generation, text },
        };
        self.wayland(cmd);
    }

    fn on_wayland(&mut self, event: WaylandEvent) {
        match event {
            WaylandEvent::OverlayShown => {
                if let Some(id) = self.awaiting_shown.take()
                    && let Some(d) = self.dictation_mut(id)
                    && let Some(t) = d.t_toggle
                {
                    d.overlay_ms = Some(t.elapsed().as_millis() as u32);
                }
            }
            WaylandEvent::Pasted { generation } | WaylandEvent::Copied { generation } => {
                self.delivered(generation);
            }
            WaylandEvent::Failed { generation, reason } => self.insert_failed(generation, reason),
            WaylandEvent::Disconnected => {
                info!("the Wayland thread disconnected");
                self.sink = None;
                // The overlay is gone with the connection; it's sent again (and shown again) on
                // the next refresh, which isn't the `toggle` a recording waits to see.
                self.overlay = None;
                self.awaiting_shown = None;
                if let Some(inserting) = &self.inserting {
                    let generation = inserting.generation;
                    self.insert_failed(generation, "the Wayland connection closed".into());
                }
            }
        }
    }

    fn delivered(&mut self, generation: u64) {
        let Some(inserting) = self.inserting.take_if(|i| i.generation == generation) else { return };
        let delete = match &inserting.what {
            Delivering::Dictation(d) => {
                self.report(d, Some(Instant::now()));
                Some(d.id)
            }
            Delivering::Redelivery(r) => r.delete,
        };
        if let Some(id) = delete
            && let Err(err) = self.store.delete(id)
        {
            warn!("cannot delete {id}: {err}");
        }
        let files = vec![self.store.last_path()];
        let dirs = (self.store.recordings_dir().to_path_buf(), self.store.state_dir().to_path_buf());
        tokio::task::spawn_blocking(move || {
            store::sync_files(&files, &dirs.1);
            store::sync_dir(&dirs.0);
        });
        self.notice(OverlayState::Done, self.t.done_shown);
    }

    fn insert_failed(&mut self, generation: u64, reason: String) {
        let Some(inserting) = self.inserting.take_if(|i| i.generation == generation) else { return };
        warn!("inserting failed: {reason}");
        let hint = match &inserting.what {
            Delivering::Dictation(d) => format!("psst paste-last --session {}", d.id),
            Delivering::Redelivery(_) => "psst paste-last".into(),
        };
        self.error_notice(&format!("paste failed: run {hint}"));
    }

    /// Sends `report` with the client's timings, and keeps them for `status`.
    fn report(&mut self, d: &Dictation, dispatched: Option<Instant>) {
        let ms = |from: Option<Instant>, to: Option<Instant>| match (from, to) {
            (Some(from), Some(to)) => Some(to.saturating_duration_since(from).as_millis() as u32),
            _ => None,
        };
        let timings = ClientTimings {
            toggle_to_overlay_ms: d.overlay_ms,
            toggle_to_first_frame_ms: ms(d.t_toggle, d.t_first_frame),
            stop_to_final_ms: ms(d.t_stop, d.t_final),
            final_to_dispatch_ms: ms(d.t_final, dispatched),
            stop_to_dispatch_ms: ms(d.t_stop, dispatched),
        };
        self.last_timings = Some(timings);
        if let (Some(client), Some(result)) = (&self.client, &d.result) {
            client.report(d.id, result.history_id, timings);
        }
    }

    // ---- the overlay and the Wayland seam ----

    fn wayland(&mut self, cmd: WaylandCmd) {
        if self.sink.is_none() {
            match self.deps.wayland.connect(self.wayland_tx.clone()) {
                Ok(sink) => self.sink = Some(sink),
                Err(reason) => {
                    debug!("no Wayland: {reason}");
                    if let WaylandCmd::Paste { generation, .. } | WaylandCmd::Copy { generation, .. } = cmd {
                        let _ = self.wayland_tx.send(WaylandEvent::Failed { generation, reason });
                    }
                    return;
                }
            }
        }
        if let Some(sink) = &mut self.sink {
            sink.send(cmd);
        }
    }

    /// Whether the last overlay command showed it.
    fn overlay_visible(&self) -> bool {
        matches!(self.overlay, Some(Some(_)))
    }

    fn notice(&mut self, state: OverlayState, shown: Duration) {
        self.notice = Some((state, Instant::now() + shown));
    }

    fn flash(&mut self, message: &str) {
        self.notice(
            OverlayState::Flash { message: message.into() },
            self.t.notice_shown.min(Duration::from_millis(1500)),
        );
    }

    fn error_notice(&mut self, message: &str) {
        self.notice(OverlayState::Error { message: message.into() }, self.t.notice_shown);
        self.sound();
    }

    fn sound(&self) {
        if self.cfg.sounds {
            (self.deps.sound)();
        }
    }

    fn desired_overlay(&self) -> Option<OverlayState> {
        let now = Instant::now();
        let notice = self.notice.as_ref().filter(|(_, until)| *until > now).map(|(state, _)| state.clone());
        if let Some(rec) = &self.recording {
            if let Some(notice) = notice {
                let message = match notice {
                    OverlayState::Flash { message } | OverlayState::Error { message } => message,
                    OverlayState::Done | OverlayState::NothingHeard => return self.recording_overlay(rec),
                    _ => String::new(),
                };
                return Some(OverlayState::Flash { message });
            }
            return self.recording_overlay(rec);
        }
        if notice.is_some() {
            return notice;
        }
        if self.inserting.is_some() {
            return Some(OverlayState::Processing);
        }
        let d = self.pipeline.front()?;
        let from_progress = |progress: Option<ProgressState>| match progress {
            Some(ProgressState::Queued) => OverlayState::WaitingForServer,
            Some(ProgressState::Loading) => OverlayState::LoadingModel,
            Some(ProgressState::Retrying) => OverlayState::Retrying,
            _ => OverlayState::Processing,
        };
        Some(match d.phase {
            Phase::Uploading if d.session.is_none() => OverlayState::WaitingForServer,
            Phase::Uploading => match d.progress {
                None | Some(ProgressState::Recording) => OverlayState::Sending,
                other => from_progress(other),
            },
            _ => from_progress(d.progress),
        })
    }

    fn recording_overlay(&self, rec: &Dictation) -> Option<OverlayState> {
        if rec.phase == Phase::Starting {
            return Some(OverlayState::Starting);
        }
        let elapsed_s = rec.frames * FRAME_MS as u64 / 1000;
        let left = (rec.max_recording_s as u64).saturating_sub(elapsed_s);
        if left <= 30 {
            return Some(OverlayState::LimitWarning { seconds_left: left as u32 });
        }
        Some(match &rec.net {
            Net::Offline(reason) => OverlayState::Offline { level: rec.level, reason: reason.clone() },
            _ => OverlayState::Recording { level: rec.level },
        })
    }

    fn refresh_overlay(&mut self) {
        let desired = self.desired_overlay();
        let current = self.overlay.as_ref();
        if current == Some(&desired) {
            return;
        }
        // Level changes alone are sent at most every 50 ms.
        let level_only = match (current, &desired) {
            (Some(Some(OverlayState::Recording { .. })), Some(OverlayState::Recording { .. })) => true,
            (Some(Some(OverlayState::Offline { reason: a, .. })), Some(OverlayState::Offline { reason: b, .. })) => {
                a == b
            }
            _ => false,
        };
        if level_only && self.overlay_at.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.overlay = Some(desired.clone());
        self.overlay_at = Instant::now();
        self.wayland(WaylandCmd::Overlay(desired));
    }

    // ---- time ----

    fn next_deadline(&self) -> Option<Instant> {
        let mut deadlines = Vec::new();
        if let Some(rec) = &self.recording {
            match rec.net {
                Net::Checking { deadline } => deadlines.push(deadline),
                Net::Live { ready: false, since } => deadlines.push(since + self.t.ready_timeout),
                _ => {}
            }
        }
        for d in &self.pipeline {
            if d.phase == Phase::Uploading && d.session.is_none() {
                deadlines.extend(d.window_end);
                if self.client.is_none() && self.attempt.is_none() {
                    deadlines.push(d.next_attempt);
                }
            }
        }
        deadlines.extend(self.notice.as_ref().map(|(_, until)| *until));
        deadlines.extend(self.stopping.as_ref().map(|(_, until)| *until));
        deadlines.into_iter().min()
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if let Some(rec) = &self.recording {
            let id = rec.id;
            match rec.net {
                Net::Checking { deadline } if now >= deadline => {
                    let reason = self.conn_error.clone().unwrap_or_else(|| "the server didn't answer in time".into());
                    self.went_offline(id, reason);
                }
                Net::Live { ready: false, since } if now >= since + self.t.ready_timeout => {
                    if let Some(rec) = &mut self.recording
                        && let Some(session) = rec.session.take()
                    {
                        let _ = session.ctl.send(Ctl::Cancel);
                    }
                    self.went_offline(id, "the server didn't answer in time".into());
                }
                _ => {}
            }
        }
        if self.notice.as_ref().is_some_and(|(_, until)| *until <= now) {
            self.notice = None;
        }
    }

    /// Moves everything along that can move: hands the connection to the next session, runs the
    /// upload windows, delivers results in order, and updates the overlay.
    fn pump(&mut self) {
        let now = Instant::now();
        if let Some(mut client) = self.usable_client()
            && self.slot.is_none()
        {
            if let Some(index) = self.pipeline.iter().position(|d| d.phase == Phase::Uploading && d.session.is_none()) {
                self.start_upload(&mut client, index);
            } else if self.recording.as_ref().is_some_and(|r| r.net == Net::Pending) {
                self.start_live(&mut client);
            }
        }
        let waiting: Vec<SessionId> =
            self.pipeline.iter().filter(|d| d.phase == Phase::Uploading && d.session.is_none()).map(|d| d.id).collect();
        for id in waiting {
            let has_client = self.client.is_some();
            let attempting = self.attempt.is_some();
            let Some(pos) = self.pipeline.iter().position(|d| d.id == id) else { continue };
            let d = &mut self.pipeline[pos];
            let window_end = d.window_end.unwrap_or(now);
            if has_client {
                continue;
            }
            if now >= window_end && !attempting {
                let d = self.pipeline.remove(pos).expect("in range");
                self.saved(d, "no connection to the server".into());
                continue;
            }
            if !attempting && now >= d.next_attempt {
                let budget = self.t.attempt_budget.min(window_end.saturating_duration_since(now));
                d.next_attempt = now + d.backoff;
                d.backoff = (d.backoff * 2).min(Duration::from_secs(2));
                self.connect(budget.max(Duration::from_millis(50)), AttemptKind::Connect);
            }
        }
        self.deliver_next();
        self.refresh_overlay();
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }
}
