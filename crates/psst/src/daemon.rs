use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use jiff::{Timestamp, Zoned};
use psst_client::{
    AudioCodec, AudioFrame, Client, ClientError, FRAME_BUFFER, FrameSender, OpusSettings, Outdated, Session,
    SessionEvent, StartOpts, TrySendError,
    api::{ApiCallError, ApiClient, Method},
    upload_file,
};
use psst_config::{ClientConfig, PasteAction, Sounds};
use psst_proto::{
    ClientTimings, DeviceId, Endpoint, ErrorCode, FRAME_MS, SessionId, Token,
    api::{HistoryRow, PairRedeemed, routes},
    ws::{FinalMsg, Mode, ProgressState},
};
use psst_wayland::{
    Chord, HistoryEntry, Listing, MenuAction, MenuOutcome, MenuSpec, ModelChoice, OverlayState, WaylandCmd,
    WaylandEvent, post_processing_label,
};
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
    menu, niri,
    pair::Credentials,
    store::{self, Last, Meta, RecState, Retained, STORAGE_BUDGET, Store},
    ui::{Wayland, WaylandSink},
};

#[derive(Debug, Clone)]
pub struct Timing {
    pub ready_timeout: Duration,
    pub stop_window: Duration,
    pub start_budget: Duration,
    pub resume_budget: Duration,
    pub resume_window: Duration,
    pub pong: Duration,
    pub attempt_budget: Duration,
    pub first_backoff: Duration,
    pub done_shown: Duration,
    pub notice_shown: Duration,
    pub niri_lookup: Duration,
    pub follow_interval: Duration,
    pub stop_chime_wait: Duration,
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
            stop_chime_wait: Duration::from_millis(100),
        }
    }
}

const OPENING_WEB_UI: &str = "Opening the web UI\u{2026}";

/// Caps for slow links, so a dead server is still noticed quickly.
const MAX_PONG: Duration = Duration::from_secs(2);
const MAX_CHECK: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// The microphone is live.
    Start,
    /// The microphone is off. Never with [`Sound::Error`]: a failure right after plays only that.
    Stop,
    /// Something went wrong, or nothing was heard.
    Error,
}

pub struct Deps {
    pub audio: Arc<dyn AudioSource>,
    pub wayland: Box<dyn Wayland>,
    pub sound: Arc<dyn Fn(Sound) + Send + Sync>,
    pub niri_socket: Option<PathBuf>,
    pub timing: Timing,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

pub struct DaemonHandle {
    tx: mpsc::UnboundedSender<Input>,
    task: tokio::task::JoinHandle<()>,
    listener: tokio::task::JoinHandle<()>,
}

impl DaemonHandle {
    pub async fn request(&self, request: Request) -> Response {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Request(request, tx)).is_err() {
            return Response::err("the daemon has stopped");
        }
        rx.await.unwrap_or_else(|_| Response::err("the daemon has stopped"))
    }

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

pub async fn spawn(cfg: ClientConfig, paths: Paths, deps: Deps) -> anyhow::Result<DaemonHandle> {
    // The socket first: it's what proves no other daemon is using the recordings.
    let listener = bind_socket(&paths.socket)?;
    let store = Store::open(&paths.state_dir)?;
    let profile = menu::load_profile(&paths.state_dir, &cfg.profile).unwrap_or_else(|| cfg.profile.clone());
    let post_processing = menu::load_post_processing(&paths.state_dir, &cfg.post_processing)
        .unwrap_or_else(|| cfg.post_processing.clone());
    let recovery = store.recover(&profile)?;
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
    let mut core = Core::new(cfg, (profile, post_processing), store, deps, (tx.clone(), wayland_tx), paths);
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

/// The watch channel keeps only the latest status, so a follower that stops reading stalls only
/// this task, never the daemon.
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
        result: Result<Client, ConnFailure>,
        fresh: bool,
    },
    /// Sent as soon as it's known: the attempt may run out of time before it ends.
    TokenRefused(Token),
    ConnClosed {
        epoch: u64,
        reason: String,
    },
    Focus {
        generation: u64,
        app_id: Option<String>,
    },
    MenuFact(menu::Fact),
    MenuDone(menu::Done),
    Resumed,
    Follow(oneshot::Sender<watch::Receiver<Status>>),
    Shutdown(oneshot::Sender<()>),
}

struct ConnFailure {
    reason: String,
    outdated: Option<Outdated>,
}

impl ConnFailure {
    fn new(err: ClientError, pairing: Option<String>) -> ConnFailure {
        let outdated = match &err {
            ClientError::Incompatible(incompatible) => Some(incompatible.outdated()),
            _ => None,
        };
        let reason = match pairing {
            Some(pairing) if unauthorized(&err) => format!("{err}; pairing by tailnet identity failed: {pairing}"),
            _ => err.to_string(),
        };
        ConnFailure { reason, outdated }
    }
}

fn unauthorized(err: &ClientError) -> bool {
    matches!(err, ClientError::Refused(refusal) if refusal.code == ErrorCode::Unauthorized)
}

/// Pairs by tailnet identity when there's no token, or when the server refuses the one there is:
/// it was revoked, or the server lost its database. A server whose `auto_pair` doesn't list this
/// machine refuses, and the connection fails as it would have. A token refused before is tried
/// after pairing, not before: on a slow link, the refused connection alone can use up an attempt's
/// budget.
async fn connect_or_pair(
    endpoint: &Endpoint,
    saved: Option<Token>,
    refused_before: bool,
    credentials: &Path,
    on_refused: impl FnOnce(Token),
) -> Result<Client, ConnFailure> {
    let mut token = saved.clone();
    let mut pairing = None;
    if token.is_none() || refused_before {
        match pair_itself(endpoint, credentials).await {
            Ok(paired) => token = Some(paired),
            Err(err) => pairing = Some(err),
        }
    }
    let mut result = Client::connect(endpoint, token.as_ref()).await;
    if let (Some(saved), Err(err)) = (saved, &result)
        && token.as_ref() == Some(&saved)
        && unauthorized(err)
    {
        on_refused(saved);
        if !refused_before {
            match pair_itself(endpoint, credentials).await {
                Ok(paired) => result = Client::connect(endpoint, Some(&paired)).await,
                Err(err) => pairing = Some(err),
            }
        }
    }
    result.map_err(|err| ConnFailure::new(err, pairing))
}

async fn pair_itself(endpoint: &Endpoint, credentials: &Path) -> Result<Token, String> {
    let api = ApiClient::new(endpoint.clone(), None);
    let paired: PairRedeemed =
        api.call(Method::Post, routes::PAIR_AUTO, None::<&()>).await.map_err(|err| match err {
            ApiCallError::Api { error, .. } => error.message,
            other => other.to_string(),
        })?;
    let creds = Credentials { server: endpoint.clone(), device_id: paired.device_id, token: paired.token };
    creds.save(credentials).map_err(|err| format!("cannot save {}: {err}", credentials.display()))?;
    info!("paired with {endpoint} as device {} by this machine's tailnet identity", creds.device_id);
    Ok(creds.token)
}

/// Short enough for the overlay to show whole; `psst status` has the versions on its server line.
fn update_prompt(outdated: Outdated) -> &'static str {
    match outdated {
        Outdated::Client => "update psst",
        Outdated::Server => "update the server",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Recording,
    Draining,
    Waiting,
    Uploading,
    Ready,
}

#[derive(Debug, Clone, PartialEq)]
enum Net {
    Checking { deadline: Instant },
    Pending,
    Live { ready: bool, since: Instant },
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

/// A saved recording on its way to the server in the background. Its text is kept for the menu's
/// history, never pasted: the window it was meant for is long gone.
struct CatchUp {
    id: SessionId,
    session: SessionCtl,
    /// Cancelled to make way for a dictation; it goes again once the connection is free.
    yielded: bool,
}

struct Dictation {
    id: SessionId,
    profile: String,
    generation: u64,
    started_at: Timestamp,
    copy_only: bool,
    phase: Phase,
    recorder: Option<Recorder>,
    stop_requested: bool,
    /// While a second `toggle` would still be a double tap. The microphone runs, so nothing said
    /// is lost, but nothing shows, chimes or goes to the server: a double tap leaves no trace.
    tap_until: Option<Instant>,
    frames: u64,
    level: f32,
    max_recording_s: u32,
    samples: u64,
    gapped: bool,
    net: Net,
    session: Option<SessionCtl>,
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
            tap_until: None,
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

struct Redelivery {
    generation: u64,
    text: String,
    copy: bool,
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
    Check,
    Connect,
}

/// The quick menu, as the daemon sees it.
struct MenuState {
    /// Asked for. It waits for a paste in progress, which would otherwise land in it, and pastes
    /// wait for it to close.
    open: bool,
    /// As the Wayland thread was last told; `None` while it's closed.
    shown: Option<MenuSpec>,
    profiles: Listing,
    post_processing: Listing,
    model: Option<ModelChoice>,
    history: Listing<HistoryRow>,
}

struct Core {
    cfg: ClientConfig,
    /// The one new dictations use: client.toml's, or the one chosen in the menu since.
    profile: String,
    /// Likewise.
    post_processing: String,
    menu: MenuState,
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
    incompatible: Option<Outdated>,
    refused_token: Option<Token>,
    /// Kept from the latest connection while there is none.
    round_trip: Duration,
    attempt: Option<u64>,
    attempts: u64,
    /// The session holding the connection. One at a time, so a recording started meanwhile keeps
    /// its frames in its backlog until the slot is free.
    slot: Option<SessionId>,
    resumed_at: Option<Instant>,
    max_recording_s: u32,
    /// This machine, as the server knows it.
    device: Option<DeviceId>,

    recording: Option<Dictation>,
    pipeline: VecDeque<Dictation>,
    redeliver: VecDeque<Redelivery>,
    inserting: Option<Inserting>,
    catch_up: Option<CatchUp>,
    /// Saved recordings that failed on this connection: sent again on the next, or when the menu
    /// opens.
    tried: HashSet<SessionId>,
    /// The store's revision when it had nothing left to send.
    nothing_to_send: Option<u64>,
    generation: u64,
    seq: u64,

    sink: Option<Box<dyn WaylandSink>>,
    overlay: Option<Option<OverlayState>>,
    overlay_at: Instant,
    notice: Option<(OverlayState, Instant)>,
    /// Held briefly after a stop, so an error that follows at once (nothing heard) plays alone. The
    /// text going in plays it, or the wait running out.
    stop_chime: Option<(SessionId, Instant)>,
    awaiting_shown: Option<SessionId>,
    last_timings: Option<ClientTimings>,
    stopping: Option<(oneshot::Sender<()>, Instant)>,

    follow: watch::Sender<Status>,
    published: Option<(Status, u64)>,
}

impl Core {
    fn new(
        cfg: ClientConfig,
        (profile, post_processing): (String, String),
        store: Store,
        deps: Deps,
        (tx, wayland_tx): (mpsc::UnboundedSender<Input>, mpsc::UnboundedSender<WaylandEvent>),
        paths: Paths,
    ) -> Core {
        let t = deps.timing.clone();
        Core {
            cfg,
            profile,
            post_processing,
            menu: MenuState {
                open: false,
                shown: None,
                profiles: Listing::Loading,
                post_processing: Listing::Loading,
                model: None,
                history: Listing::Loading,
            },
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
            incompatible: None,
            refused_token: None,
            round_trip: Duration::ZERO,
            attempt: None,
            attempts: 0,
            slot: None,
            resumed_at: None,
            max_recording_s: 600,
            device: None,
            recording: None,
            pipeline: VecDeque::new(),
            redeliver: VecDeque::new(),
            inserting: None,
            catch_up: None,
            tried: HashSet::new(),
            nothing_to_send: None,
            generation: 0,
            seq: 0,
            sink: None,
            overlay: None,
            overlay_at: Instant::now(),
            notice: None,
            stop_chime: None,
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
            Input::TokenRefused(token) => self.refused_token = Some(token),
            Input::ConnClosed { epoch, reason } => self.on_conn_closed(epoch, reason),
            Input::Focus { generation, app_id } => self.on_focus(generation, app_id),
            Input::MenuFact(fact) => self.on_menu_fact(fact),
            Input::MenuDone(done) => self.on_menu_done(done),
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

    fn on_request(&mut self, request: Request) -> Response {
        if self.stopping.is_some() {
            return Response::err("the daemon is stopping");
        }
        match request {
            Request::Toggle { .. } if self.menu.open => self.close_menu(),
            Request::Toggle { profile } => match &self.recording {
                Some(rec) if rec.tap_until.is_some() => self.double_tap(),
                Some(_) => self.stop_recording(),
                None => self.start_recording(profile, Duration::from_millis(self.cfg.menu.double_tap_ms.into())),
            },
            Request::Start => {
                self.menu.open = false;
                match self.recording {
                    Some(_) => {
                        self.flash("already recording");
                        Response::err("already recording")
                    }
                    None => self.start_recording(None, Duration::ZERO),
                }
            }
            Request::Menu if self.menu.open => self.close_menu(),
            // The menu would hide the pill while the microphone is still live.
            Request::Menu if self.recording.is_some() => {
                self.flash("stop the recording first");
                Response::err("stop the recording first")
            }
            Request::Menu => self.open_menu(),
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

    fn start_recording(&mut self, profile: Option<String>, tap_window: Duration) -> Response {
        let usage = self.store.usage();
        if usage > STORAGE_BUDGET {
            let message = format!("saved recordings use {} MB: see psst status", usage / 1_000_000);
            self.error_notice(&message);
            return Response::err(message);
        }
        let id = SessionId::new_v4();
        let generation = self.next_generation();
        let profile = profile.unwrap_or_else(|| self.profile.clone());
        let mut d = Dictation::new(id, profile, generation, Phase::Starting, Net::Pending);
        d.t_toggle = Some(Instant::now());
        d.tap_until = (!tap_window.is_zero()).then(|| Instant::now() + tap_window);
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
        // `OverlayShown` comes only when the overlay appears, so with one already up (a notice, or
        // the previous dictation finishing) the time isn't measured.
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
        self.stop_chime = Some((id, Instant::now() + self.t.stop_chime_wait));
        info!("stopped {id}");
        Response::ok(format!("stopped {id}"))
    }

    /// The first tap started a recording; it's thrown away unheard.
    fn double_tap(&mut self) -> Response {
        let d = self.recording.take().expect("checked by the caller");
        info!("double tap: dropped {} to open the menu", d.id);
        self.awaiting_shown = None;
        self.drop_dictation(d);
        self.open_menu()
    }

    fn open_menu(&mut self) -> Response {
        if let Err(reason) = self.connect_wayland() {
            return Response::err(format!("cannot show the menu: {reason}"));
        }
        self.menu.open = true;
        if matches!(self.menu.profiles, Listing::Failed(_)) {
            self.menu.profiles = Listing::Loading;
        }
        if matches!(self.menu.history, Listing::Failed(_)) {
            self.menu.history = Listing::Loading;
        }
        if matches!(self.menu.post_processing, Listing::Failed(_)) {
            self.menu.post_processing = Listing::Loading;
        }
        self.fetch_menu_facts();
        // Looking at the history is a good moment to send what's saved, failed before or not.
        self.forget_tried();
        if !self.client.as_ref().is_some_and(|client| !client.is_closed()) {
            self.connect(self.check_budget(self.t.resume_budget), AttemptKind::Connect);
        }
        Response::ok("opened the menu")
    }

    fn fetch_menu_facts(&self) {
        let tx = self.tx.clone();
        menu::fetch(self.api(), self.device(), move |fact| {
            let _ = tx.send(Input::MenuFact(fact));
        });
    }

    fn device(&self) -> Option<DeviceId> {
        if self.device.is_some() || !self.endpoint_is_tcp() {
            return self.device.clone();
        }
        Credentials::load(&self.credentials).ok().flatten().map(|creds| creds.device_id)
    }

    fn close_menu(&mut self) -> Response {
        self.menu.open = false;
        Response::ok("closed the menu")
    }

    fn on_menu_fact(&mut self, fact: menu::Fact) {
        match fact {
            menu::Fact::Profiles(Ok(names)) => self.menu.profiles = Listing::Ready(names),
            // Switching needs only the names, so a list from before still does.
            menu::Fact::Profiles(Err(_)) if matches!(self.menu.profiles, Listing::Ready(_)) => {}
            menu::Fact::Profiles(Err(reason)) => self.menu.profiles = Listing::Failed(reason),
            menu::Fact::PostProcessing(Ok(styles)) => self.menu.post_processing = Listing::Ready(styles),
            menu::Fact::PostProcessing(Err(_)) if matches!(self.menu.post_processing, Listing::Ready(_)) => {}
            menu::Fact::PostProcessing(Err(reason)) => self.menu.post_processing = Listing::Failed(reason),
            menu::Fact::Model(state) => self.menu.model = Some(menu::choice(state)),
            menu::Fact::History(Ok(rows)) => self.menu.history = Listing::Ready(rows),
            menu::Fact::History(Err(_)) if matches!(self.menu.history, Listing::Ready(_)) => {}
            menu::Fact::History(Err(reason)) => self.menu.history = Listing::Failed(reason),
        }
    }

    fn menu_action(&mut self, action: MenuAction) {
        info!("menu: {action:?}");
        let progress = match &action {
            MenuAction::SwitchProfile(name) => return self.switch_profile(name.clone()),
            MenuAction::SwitchPostProcessing(style) => return self.switch_post_processing(style.clone()),
            MenuAction::Paste(key) | MenuAction::Copy(key) | MenuAction::Discard(key) => {
                let response = self.history_action(key, &action);
                if !response.ok {
                    self.error_notice(&response.message);
                }
                return;
            }
            MenuAction::AddWord(word) => format!("Adding \u{201c}{word}\u{201d}\u{2026}"),
            MenuAction::UnloadModel => "Unloading the model\u{2026}".into(),
            MenuAction::LoadModel => "Loading the model\u{2026}".into(),
            MenuAction::OpenWebUi => OPENING_WEB_UI.into(),
        };
        self.flash(&progress);
        let (api, tx) = (self.api(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Input::MenuDone(menu::act(api, action).await));
        });
    }

    fn on_menu_done(&mut self, done: menu::Done) {
        if let Some(state) = done.model {
            self.menu.model = Some(menu::choice(state));
        }
        match done.result {
            Ok(Some(message)) => self.flash(&message),
            // The browser opening says it.
            Ok(None) => {
                self.notice.take_if(
                    |(state, _)| matches!(state, OverlayState::Flash { message } if message == OPENING_WEB_UI),
                );
            }
            Err(message) => self.error_notice(&message),
        }
    }

    fn history_action(&mut self, key: &str, action: &MenuAction) -> Response {
        let Ok(id) = key.parse::<SessionId>() else { return Response::err(format!("no dictation {key}")) };
        let copy = match action {
            MenuAction::Discard(_) => return self.discard(id),
            MenuAction::Copy(_) => true,
            _ => false,
        };
        if self.store.read_meta(id).is_ok_and(|meta| meta.state == RecState::Transcribed) {
            return self.redeliver(Some(id), copy);
        }
        let past = match &self.menu.history {
            Listing::Ready(rows) => rows.iter().find(|row| row.session_id == id),
            _ => None,
        };
        match past.and_then(|row| row.r#final.as_ref()) {
            Some(fin) => self.queue_redelivery(fin.text.clone(), copy, None),
            None => Response::err(format!("no dictation {id}")),
        }
    }

    fn switch_profile(&mut self, name: String) {
        if let Err(err) = menu::save_profile(self.store.state_dir(), &self.cfg.profile, &name) {
            warn!("cannot keep profile {name} for the next start: {err}");
        }
        info!("profile: {name}");
        self.flash(&format!("Profile: {name}"));
        self.profile = name;
    }

    fn switch_post_processing(&mut self, style: String) {
        if let Err(err) = menu::save_post_processing(self.store.state_dir(), &self.cfg.post_processing, &style) {
            warn!("cannot keep post-processing {style} for the next start: {err}");
        }
        info!("post-processing: {style}");
        self.flash(&format!("Post-processing: {}", post_processing_label(&style)));
        self.post_processing = style;
    }

    fn sent_post_processing(&self) -> Option<String> {
        Some(self.post_processing.clone()).filter(|style| style != "none")
    }

    fn refresh_menu(&mut self) {
        let wanted = (self.menu.open && self.inserting.is_none()).then(|| self.menu_spec());
        if wanted != self.menu.shown {
            self.menu.shown = wanted.clone();
            self.wayland(WaylandCmd::Menu(wanted));
        }
    }

    fn menu_spec(&self) -> MenuSpec {
        let now = Zoned::now();
        let saved: Vec<Retained> = (self.store.list().into_iter().rev())
            .filter(|r| r.meta.state != RecState::Recording && !self.foreground(r.id))
            .collect();
        let entry = |r: &Retained| {
            let when = menu::when(r.meta.started_at, &now);
            match (r.meta.state, &r.meta.final_text) {
                (RecState::Transcribed, Some(text)) => {
                    HistoryEntry { key: r.id.to_string(), when, text: Some(text.trim().to_string()) }
                }
                _ => {
                    let when = format!("{when} \u{b7} {}", menu::length(r.meta.samples));
                    HistoryEntry { key: r.id.to_string(), when, text: None }
                }
            }
        };
        let history = match &self.menu.history {
            Listing::Loading => Listing::Loading,
            Listing::Failed(reason) => Listing::Failed(reason.clone()),
            Listing::Ready(rows) => Listing::Ready(
                rows.iter()
                    .filter(|row| !saved.iter().any(|r| r.id == row.session_id) && !self.foreground(row.session_id))
                    .filter_map(|row| {
                        let text = row.r#final.as_ref()?.text.trim().to_string();
                        Some(HistoryEntry {
                            key: row.session_id.to_string(),
                            when: menu::when(row.created_at, &now),
                            text: Some(text),
                        })
                    })
                    .collect(),
            ),
        };
        MenuSpec {
            profile: self.profile.clone(),
            post_processing: self.post_processing.clone(),
            post_processing_styles: self.menu.post_processing.clone(),
            profiles: self.menu.profiles.clone(),
            model: self.menu.model,
            saved: saved.iter().map(entry).collect(),
            history,
        }
    }

    fn api(&self) -> ApiClient {
        ApiClient::new(self.cfg.server.clone(), self.token())
    }

    fn token(&self) -> Option<Token> {
        if !self.endpoint_is_tcp() {
            return None;
        }
        match Credentials::load(&self.credentials) {
            Ok(creds) => creds.map(|creds| creds.token),
            Err(err) => {
                warn!("cannot read {}: {err}", self.credentials.display());
                None
            }
        }
    }

    fn cancel(&mut self) -> Response {
        let Some(d) = self.recording.take().or_else(|| self.pipeline.pop_back()) else {
            return Response::err("nothing to cancel");
        };
        let id = d.id;
        self.drop_dictation(d);
        info!("cancelled {id}");
        Response::ok(format!("cancelled {id}"))
    }

    fn drop_dictation(&mut self, d: Dictation) {
        self.stop_chime.take_if(|(id, _)| *id == d.id);
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

    /// Recording, or on its way to the window it was meant for.
    fn foreground(&self, id: SessionId) -> bool {
        self.recording.as_ref().is_some_and(|d| d.id == id)
            || self.pipeline.iter().any(|d| d.id == id)
            || matches!(&self.inserting, Some(Inserting { what: Delivering::Dictation(d), .. }) if d.id == id)
    }

    fn in_flight(&self, id: SessionId) -> bool {
        self.foreground(id) || self.catch_up.as_ref().is_some_and(|c| c.id == id)
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
        self.queue_redelivery(text, copy, delete)
    }

    fn queue_redelivery(&mut self, text: String, copy: bool, delete: Option<SessionId>) -> Response {
        let generation = self.next_generation();
        self.wayland(WaylandCmd::SnapshotClipboard { generation });
        self.redeliver.push_back(Redelivery { generation, text, copy, delete });
        Response::ok(if copy { "copying" } else { "pasting" })
    }

    fn discard(&mut self, id: SessionId) -> Response {
        if self.foreground(id) {
            return Response::err(format!("{id} is in progress: use psst cancel"));
        }
        if let Some(catch_up) = self.catch_up.take_if(|c| c.id == id) {
            let _ = catch_up.session.ctl.send(Ctl::Cancel);
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
            profile: self.profile.clone(),
            post_processing: self.post_processing.clone(),
            connection,
            error,
            recording: self.recording.as_ref().map(info),
            finishing: self.pipeline.iter().map(info).collect(),
            retained: Vec::new(),
            warnings: self.incompatible.map(|o| vec![update_prompt(o).to_string()]).unwrap_or_default(),
            last_timings: self.last_timings,
        }
    }

    fn complete_status(&self, mut summary: Status) -> Status {
        let retained = self.store.list();
        summary.warnings.extend(self.store.warnings(&retained));
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

    /// Runs after every input, audio frames included, so the recordings directory is read only when
    /// the cheap summary or the store's revision changed.
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

    fn endpoint_is_tcp(&self) -> bool {
        matches!(self.cfg.server, Endpoint::Tcp { .. })
    }

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

    fn pong_budget(&self) -> Duration {
        self.t.pong.max(self.round_trip.saturating_mul(3).min(MAX_PONG))
    }

    /// Room for the pong wait and a reconnect: TCP, the WebSocket upgrade, `hello`/`welcome`, and
    /// a round trip to spare.
    fn check_budget(&self, floor: Duration) -> Duration {
        floor.max((self.pong_budget() + self.round_trip.saturating_mul(4)).min(MAX_CHECK))
    }

    /// No reconnect timers: attempts are made only at start-up, once after a close or resume, at
    /// `start`, and in an upload's window after `stop`.
    fn connect(&mut self, budget: Duration, kind: AttemptKind) {
        if self.attempt.is_some() {
            return;
        }
        self.attempts += 1;
        let attempt = self.attempts;
        self.attempt = Some(attempt);
        let endpoint = self.cfg.server.clone();
        let (token, credentials) = match self.endpoint_is_tcp() {
            true => (self.token(), Some(self.credentials.clone())),
            false => (None, None),
        };
        let refused_before = token.is_some() && token == self.refused_token;
        let refused_tx = self.tx.clone();
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
                let result = match &credentials {
                    Some(credentials) => {
                        let refused = |token| drop(refused_tx.send(Input::TokenRefused(token)));
                        connect_or_pair(&endpoint, token, refused_before, credentials, refused).await
                    }
                    None => Client::connect(&endpoint, None).await.map_err(|err| ConnFailure::new(err, None)),
                };
                (result, true)
            };
            let (result, fresh) = match tokio::time::timeout(budget, work).await {
                Ok(done) => done,
                Err(_) => {
                    let reason = format!("no answer from {endpoint} within {} ms", budget.as_millis());
                    (Err(ConnFailure { reason, outdated: None }), true)
                }
            };
            let _ = tx.send(Input::Conn { attempt, result, fresh });
        });
    }

    fn on_conn(&mut self, attempt: u64, result: Result<Client, ConnFailure>, fresh: bool) {
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
                    self.device = Some(client.welcome().device.clone());
                    self.client = Some(client);
                    self.connected_at = Instant::now();
                    // So the menu has them the first time it opens; each opening refreshes them,
                    // and an open one that couldn't reach the server gets them now.
                    let first = self.menu.profiles == Listing::Loading && self.menu.model.is_none();
                    if first || self.menu.open {
                        self.fetch_menu_facts();
                    }
                    self.forget_tried();
                    // A session on the old connection is lost with it.
                    self.slot = None;
                }
                self.conn_error = None;
                self.incompatible = None;
                self.refused_token = None;
                if let Some(rec) = &mut self.recording
                    && matches!(rec.net, Net::Checking { .. })
                {
                    rec.net = Net::Pending;
                }
            }
            Err(ConnFailure { reason, outdated }) => {
                if fresh && let Some(old) = self.client.take() {
                    old.close();
                }
                self.conn_error = Some(reason.clone());
                let reason = match outdated {
                    Some(outdated) => {
                        warn!("{}: {reason}", update_prompt(outdated));
                        self.refused(outdated);
                        update_prompt(outdated).to_string()
                    }
                    None => {
                        info!("cannot reach the server: {reason}");
                        reason
                    }
                };
                if let Some(rec) = &self.recording
                    && matches!(rec.net, Net::Checking { .. })
                {
                    let id = rec.id;
                    self.went_offline(id, reason);
                }
            }
        }
    }

    /// Says so once, unless a dictation in flight will, and ends every waiting upload's window:
    /// retrying can't help until one side is updated.
    fn refused(&mut self, outdated: Outdated) {
        let learned = self.incompatible != Some(outdated);
        self.incompatible = Some(outdated);
        if learned && self.recording.is_none() && self.pipeline.is_empty() {
            self.error_notice(update_prompt(outdated));
        }
        let now = Instant::now();
        for d in &mut self.pipeline {
            if d.phase == Phase::Uploading && d.session.is_none() {
                d.window_end = Some(now);
            }
        }
    }

    fn unreachable_reason(&self) -> String {
        self.incompatible.map_or("no connection to the server", update_prompt).to_string()
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

    /// Opus only over TCP: the Unix socket never leaves the machine, so compressing saves nothing.
    fn codec(&self) -> AudioCodec {
        match self.cfg.audio.codec {
            psst_config::AudioCodec::Opus if self.endpoint_is_tcp() => AudioCodec::Opus(OpusSettings::default()),
            psst_config::AudioCodec::Opus | psst_config::AudioCodec::Pcm => AudioCodec::Pcm,
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
            post_processing: self.sent_post_processing(),
            session_id: id,
            profile: Some(profile),
            mode: Mode::Live,
            resume: false,
            eval: None,
            codec: self.codec(),
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
        let opts = StartOpts {
            post_processing: self.sent_post_processing(),
            session_id: id,
            profile: Some(profile),
            mode: Mode::Upload,
            resume: true,
            eval: None,
            codec: self.codec(),
        };
        let session = upload_file(client, opts, &wav);
        let ctl = self.spawn_session(id, session);
        let d = &mut self.pipeline[index];
        d.session = Some(ctl);
        d.progress = None;
        self.slot = Some(id);
        info!("uploading {id}");
    }

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

    /// The recording goes on to its file: neither the network nor the server stops the mic.
    fn went_offline(&mut self, id: SessionId, reason: String) {
        let Some(d) = self.dictation_mut(id) else { return };
        if matches!(d.net, Net::Offline(_)) {
            return;
        }
        info!("{id} is offline: {reason}");
        d.net = Net::Offline(reason);
        d.backlog = Vec::new();
        let recording = matches!(d.phase, Phase::Starting | Phase::Recording);
        if recording && d.tap_until.is_none() {
            self.sound(Sound::Error);
        }
    }

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
        let live = first && d.phase == Phase::Starting;
        let quiet = d.tap_until.is_some();
        if first {
            d.t_first_frame = Some(Instant::now());
            if live {
                d.phase = Phase::Recording;
            }
        }
        let mut lost = None;
        match (&d.net, &d.session) {
            (Net::Live { .. }, Some(session)) => match session.frames.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full) => lost = Some(("the server isn't keeping up".to_string(), true)),
                // The client closes the channel before the session's last event reaches us, and
                // that event says why it ended (a rejection's reason, say). The frame is in the file.
                Err(TrySendError::Closed) => {}
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
        if live && is_recording && !quiet {
            self.sound(Sound::Start);
        }
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
                    let reason = self.unreachable_reason();
                    self.saved(d, reason);
                }
            }
        }
    }

    fn on_session(&mut self, id: SessionId, seq: u64, event: SessionEvent) {
        let terminal = event.is_terminal();
        if terminal && self.slot == Some(id) {
            self.slot = None;
        }
        if self.catch_up.as_ref().is_some_and(|c| c.id == id && c.session.seq == seq) {
            if terminal {
                self.caught_up(id, event);
            }
            return;
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

    fn caught_up(&mut self, id: SessionId, event: SessionEvent) {
        let catch_up = self.catch_up.take().expect("checked by the caller");
        self.nothing_to_send = None;
        let text = match event {
            SessionEvent::Final(msg) => msg.r#final.text,
            SessionEvent::Cancelled if catch_up.yielded => return,
            other => {
                warn!("{id} failed in the background: {other:?}");
                self.tried.insert(id);
                return;
            }
        };
        if text.trim().is_empty() {
            info!("{id}: nothing heard");
            if let Err(err) = self.store.delete(id) {
                warn!("cannot delete {id}: {err}");
            }
            return;
        }
        info!("{id} transcribed in the background");
        let saved = self.store.read_meta(id).and_then(|mut meta| {
            meta.state = RecState::Transcribed;
            meta.final_text = Some(text);
            // No `finalized_at`: `recover` would make this the text `paste-last` pastes, which
            // is the latest dictation's.
            self.store.write_meta(id, &meta)
        });
        if let Err(err) = saved {
            warn!("cannot save the text of {id}: {err}");
        }
    }

    fn forget_tried(&mut self) {
        self.tried.clear();
        self.nothing_to_send = None;
    }

    /// Oldest first.
    fn next_to_send(&mut self) -> Option<Retained> {
        let revision = self.store.revision();
        if self.nothing_to_send == Some(revision) {
            return None;
        }
        let next = self.store.list().into_iter().find(|r| {
            matches!(r.meta.state, RecState::Complete | RecState::Incomplete)
                && r.meta.samples > 0
                && !self.in_flight(r.id)
                && !self.tried.contains(&r.id)
        });
        if next.is_none() {
            self.nothing_to_send = Some(revision);
        }
        next
    }

    fn start_catch_up(&mut self, client: &mut Client, rec: Retained) {
        let id = rec.id;
        let opts = StartOpts {
            session_id: id,
            profile: Some(rec.meta.profile),
            post_processing: self.sent_post_processing(),
            mode: Mode::Upload,
            resume: true,
            eval: None,
            codec: self.codec(),
        };
        let session = upload_file(client, opts, &self.store.wav_path(id));
        let session = self.spawn_session(id, session);
        self.slot = Some(id);
        self.catch_up = Some(CatchUp { id, session, yielded: false });
        info!("sending {id} in the background");
    }

    /// A dictation takes the connection from one going in the background, which goes again later.
    fn yield_slot(&mut self) {
        let wanted = self.recording.as_ref().is_some_and(|r| r.tap_until.is_none()) || !self.pipeline.is_empty();
        if let Some(catch_up) = self.catch_up.as_mut().filter(|c| wanted && !c.yielded && self.slot == Some(c.id)) {
            info!("{}: making way for a dictation", catch_up.id);
            let _ = catch_up.session.ctl.send(Ctl::Cancel);
            catch_up.yielded = true;
        }
    }

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

    /// Not sent again on this connection: it just failed on it.
    fn saved(&mut self, d: Dictation, reason: String) {
        warn!("{} saved: {reason}", d.id);
        if let Some(session) = &d.session {
            let _ = session.ctl.send(Ctl::Cancel);
        }
        self.tried.insert(d.id);
        self.nothing_to_send = None;
        self.error_notice(&format!("saved to History ({reason})"));
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
            self.sound(Sound::Error);
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

    fn deliver_next(&mut self) {
        // The menu has the keyboard, so a paste would land in it.
        if self.inserting.is_some() || self.menu.open {
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
            WaylandEvent::Menu(MenuOutcome::Kept(action)) => self.menu_action(action),
            // `shown` stays: an update sent before this arrived may have opened it again, so the
            // next refresh closes it for certain.
            WaylandEvent::Menu(outcome) => {
                self.menu.open = false;
                if let MenuOutcome::Chose(action) = outcome {
                    self.menu_action(action);
                }
            }
            WaylandEvent::Disconnected => {
                info!("the Wayland thread disconnected");
                self.sink = None;
                self.menu.open = false;
                self.menu.shown = None;
                // The overlay is gone with the connection; the next refresh shows it again, which
                // isn't the `toggle` a recording waits to see.
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
                if self.stop_chime.take_if(|(id, _)| *id == d.id).is_some() {
                    self.sound(Sound::Stop);
                }
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

    fn wayland(&mut self, cmd: WaylandCmd) {
        if let Err(reason) = self.connect_wayland() {
            debug!("no Wayland: {reason}");
            if let WaylandCmd::Paste { generation, .. } | WaylandCmd::Copy { generation, .. } = cmd {
                let _ = self.wayland_tx.send(WaylandEvent::Failed { generation, reason });
            }
            return;
        }
        if let Some(sink) = &mut self.sink {
            sink.send(cmd);
        }
    }

    fn connect_wayland(&mut self) -> Result<(), String> {
        if self.sink.is_none() {
            self.sink = Some(self.deps.wayland.connect(self.wayland_tx.clone())?);
        }
        Ok(())
    }

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
        self.sound(Sound::Error);
    }

    fn sound(&mut self, sound: Sound) {
        if sound == Sound::Error {
            self.stop_chime = None;
        }
        if self.cfg.sounds != Sounds::Off {
            (self.deps.sound)(sound);
        }
    }

    fn desired_overlay(&self) -> Option<OverlayState> {
        if self.menu.shown.is_some() {
            return None;
        }
        let now = Instant::now();
        let notice = self.notice.as_ref().filter(|(_, until)| *until > now).map(|(state, _)| state.clone());
        if let Some(rec) = self.recording.as_ref().filter(|rec| rec.tap_until.is_none()) {
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
            return Some(OverlayState::LimitWarning { level: rec.level, seconds_left: left as u32 });
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
        let level_only = match (current, &desired) {
            (Some(Some(OverlayState::Recording { .. })), Some(OverlayState::Recording { .. })) => true,
            (Some(Some(OverlayState::Offline { reason: a, .. })), Some(OverlayState::Offline { reason: b, .. })) => {
                a == b
            }
            (
                Some(Some(OverlayState::LimitWarning { seconds_left: a, .. })),
                Some(OverlayState::LimitWarning { seconds_left: b, .. }),
            ) => a == b,
            _ => false,
        };
        if level_only && self.overlay_at.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.overlay = Some(desired.clone());
        self.overlay_at = Instant::now();
        self.wayland(WaylandCmd::Overlay(desired));
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut deadlines = Vec::new();
        if let Some(rec) = &self.recording {
            deadlines.extend(rec.tap_until);
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
        deadlines.extend(self.stop_chime.map(|(_, due)| due));
        deadlines.extend(self.stopping.as_ref().map(|(_, until)| *until));
        deadlines.into_iter().min()
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if let Some(rec) = &mut self.recording
            && rec.tap_until.take_if(|until| *until <= now).is_some()
        {
            let (live, offline) = (rec.phase == Phase::Recording, matches!(rec.net, Net::Offline(_)));
            if live {
                self.sound(Sound::Start);
            }
            if offline {
                self.sound(Sound::Error);
            }
        }
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
        if self.stop_chime.take_if(|(_, due)| *due <= now).is_some() {
            self.sound(Sound::Stop);
        }
    }

    fn pump(&mut self) {
        let now = Instant::now();
        if let Some(mut client) = self.usable_client()
            && self.slot.is_none()
        {
            if let Some(index) = self.pipeline.iter().position(|d| d.phase == Phase::Uploading && d.session.is_none()) {
                self.start_upload(&mut client, index);
            } else if self.recording.as_ref().is_some_and(|r| r.net == Net::Pending && r.tap_until.is_none()) {
                self.start_live(&mut client);
            } else if self.recording.is_none()
                && self.pipeline.is_empty()
                && self.stopping.is_none()
                && let Some(rec) = self.next_to_send()
            {
                self.start_catch_up(&mut client, rec);
            }
        }
        self.yield_slot();
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
                let reason = self.unreachable_reason();
                self.saved(d, reason);
                continue;
            }
            if !attempting && now >= d.next_attempt {
                let budget = self.t.attempt_budget.min(window_end.saturating_duration_since(now));
                d.next_attempt = now + d.backoff;
                d.backoff = (d.backoff * 2).min(Duration::from_secs(2));
                self.connect(budget.max(Duration::from_millis(50)), AttemptKind::Connect);
            }
        }
        // The menu closes before a paste it held back is sent.
        self.refresh_menu();
        self.deliver_next();
        self.refresh_overlay();
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }
}
