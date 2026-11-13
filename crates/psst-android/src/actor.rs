//! One task owns all the state. Connection attempts and sessions run in tasks of their own and
//! report back as [`Internal`] messages, so nothing here waits on the network.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use psst_client::{
    AudioCodec, AudioFrame, Client, FRAME_BUFFER, FrameSender, OpusSettings, Session, SessionEvent, StartOpts,
    TrySendError,
};
use psst_proto::{
    ClientTimings, Endpoint, ErrorCode, FRAME_SAMPLES, HistoryId, SAMPLE_RATE, SessionId, Token,
    ws::{FinalMsg, Mode, ProgressState, ReadyModelState},
};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    Command, Dictation, Event, HistoryItem, Link, State, Timing,
    config::Config,
    connect::{self, Failure, Learned},
    history,
};

const MAX_PONG: Duration = Duration::from_secs(2);
const MAX_CHECK: Duration = Duration::from_secs(3);
const MAX_BACKOFF: Duration = Duration::from_secs(2);
/// A connection that dies sooner isn't worth an immediate second try.
const RECONNECT_AFTER: Duration = Duration::from_secs(2);
const DEFAULT_MAX_RECORDING_S: u32 = 600;
const LIMIT_WARNING_S: u64 = 10;
const SAMPLES_PER_S: u64 = SAMPLE_RATE as u64;
/// Finals the app may still report as inserted.
const AWAITING_INSERT: usize = 8;

const OFFLINE_RECORDING: &str = "Offline, recording anyway";
const CONNECTION_DROPPED: &str = "Lost the connection to the server; psst reconnects when it needs to.";
const NOT_CONNECTED: &str = "Not connected to the server yet.";
const NO_SERVER: &str = "psst is not connected to a server yet: set it up first.";
const NOT_PAIRED: &str = "This phone is not paired with the server yet: open psst's setup to pair it.";

pub(crate) enum Input {
    Command(Command),
    Audio(Vec<i16>),
    Shutdown,
}

enum Internal {
    Conn { attempt: u64, result: Result<Client, Failure>, fresh: bool },
    Learned(Learned),
    Closed { epoch: u64, reason: String },
    Session { seq: u64, event: SessionEvent },
    History(Result<Vec<HistoryItem>, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    /// Pings the current connection, and connects again only if it doesn't answer.
    Check,
    Connect,
}

pub(crate) async fn run(
    data_dir: PathBuf,
    device_name: String,
    t: Timing,
    mut input: mpsc::UnboundedReceiver<Input>,
    events: std_mpsc::Sender<Event>,
) {
    let (tx, mut internal) = mpsc::unbounded_channel();
    let config = Config::load(&data_dir);
    let mut actor = Actor {
        endpoint: parse(config.server.as_deref()),
        config,
        data_dir,
        device_name,
        t,
        events,
        tx,
        failure: None,
        client: None,
        epoch: 0,
        connected_at: Instant::now(),
        round_trip: Duration::ZERO,
        server_version: None,
        max_recording_s: DEFAULT_MAX_RECORDING_S,
        refused_token: None,
        attempt: None,
        attempts: 0,
        seq: 0,
        job: None,
        awaiting_insert: VecDeque::new(),
        published: None,
    };
    actor.note_bad_address();
    // The process starts because a UI wants the core, so a connection is wanted now.
    actor.connect(AttemptKind::Connect, actor.t.connect_budget);
    actor.publish();
    loop {
        let deadline = actor.next_deadline();
        tokio::select! {
            biased;
            Some(msg) = internal.recv() => actor.on_internal(msg),
            input = input.recv() => match input {
                Some(Input::Command(command)) => actor.on_command(command),
                Some(Input::Audio(samples)) => actor.on_audio(&samples),
                Some(Input::Shutdown) | None => break,
            },
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {}
        }
        actor.pump();
        actor.publish();
    }
    if let Some(client) = &actor.client {
        client.close();
    }
}

fn parse(server: Option<&str>) -> Option<Result<Endpoint, String>> {
    server.map(|server| {
        server.parse().map_err(|_| format!("{server} is not a server address: enter one like http://server:7341."))
    })
}

/// `host` and `host:port` get `http://`; an address that parses is kept in its canonical form.
fn normalise(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    let full =
        if server.starts_with('/') || server.contains("://") { server.into() } else { format!("http://{server}") };
    Some(full.parse::<Endpoint>().map_or_else(|_| server.into(), |endpoint| endpoint.to_string()))
}

fn limit(max_recording_s: u32) -> u64 {
    u64::from(max_recording_s) * SAMPLES_PER_S
}

/// The last one is padded with zeros.
fn frame(audio: &[i16], index: usize) -> AudioFrame {
    let mut frame = [0; FRAME_SAMPLES];
    let chunk = &audio[index * FRAME_SAMPLES..((index + 1) * FRAME_SAMPLES).min(audio.len())];
    frame[..chunk.len()].copy_from_slice(chunk);
    frame
}

fn codec() -> AudioCodec {
    AudioCodec::Opus(OpusSettings::default())
}

struct SessionCtl {
    seq: u64,
    frames: FrameSender,
    ctl: mpsc::UnboundedSender<Ctl>,
    upload: bool,
}

enum Ctl {
    Stop(u64),
    Cancel,
}

fn spawn_session(seq: u64, mut session: Session, upload: bool, tx: mpsc::UnboundedSender<Internal>) -> SessionCtl {
    let frames = session.frames();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut ctl_open = true;
        loop {
            tokio::select! {
                event = session.next_event() => {
                    let terminal = event.is_terminal();
                    let _ = tx.send(Internal::Session { seq, event });
                    if terminal {
                        break;
                    }
                }
                ctl = ctl_rx.recv(), if ctl_open => match ctl {
                    Some(Ctl::Stop(samples)) => {
                        if let Err(err) = session.stop(samples).await {
                            debug!("stop for {}: {err}", session.id());
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
    SessionCtl { seq, frames, ctl: ctl_tx, upload }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Recording,
    /// Stopped; waiting for the final.
    Finishing,
    Failed(String),
}

enum Live {
    /// No session yet. The recording says it is offline from `offline_at`, or once an attempt
    /// failed, but a session can still start until `stop`.
    Waiting { offline_at: Instant, offline: bool },
    /// `sent` frames went to the session.
    Streaming { sent: usize },
    /// Lost, refused, or the server couldn't keep up: `stop` re-delivers the whole recording.
    Off,
}

/// The time after `stop` (or `retry`) in which the core keeps trying to re-deliver. A session in
/// progress isn't cut short by its end.
struct Window {
    end: Instant,
    next_attempt: Instant,
    backoff: Duration,
    /// An attempt failed: the server is unreachable for now.
    unreachable: bool,
}

struct Job {
    id: SessionId,
    /// Kept whole until the final, a cancel or a discard. Shared with an upload while it runs.
    audio: Arc<Vec<i16>>,
    limit: u64,
    phase: Phase,
    live: Live,
    session: Option<SessionCtl>,
    /// A session the core cancelled, until it ends. psst-client matches a cancel by session id, so
    /// one that arrived after the next session with this id started would end that one instead.
    ending: Option<u64>,
    model_loading: bool,
    progress: Option<ProgressState>,
    window: Option<Window>,
    t_start: Option<Instant>,
    t_first_audio: Option<Instant>,
    t_stop: Option<Instant>,
}

impl Job {
    /// Hands the live session the frames completed since the last call; with `flush`, also the
    /// last, partial one. `Err` says why the session can't take them, and whether the connection
    /// is too slow to keep.
    fn stream(&mut self, flush: bool) -> Result<(), (String, bool)> {
        let (Live::Streaming { sent }, Some(session)) = (&mut self.live, &self.session) else { return Ok(()) };
        let end = if flush { self.audio.len().div_ceil(FRAME_SAMPLES) } else { self.audio.len() / FRAME_SAMPLES };
        while *sent < end {
            match session.frames.try_send(frame(&self.audio, *sent)) {
                Ok(()) => *sent += 1,
                Err(TrySendError::Full) => return Err(("the server isn't keeping up".into(), true)),
                Err(TrySendError::Closed) => return Err(("the session ended".into(), false)),
            }
        }
        Ok(())
    }

    fn end_session(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session.ctl.send(Ctl::Cancel);
            self.ending = Some(session.seq);
        }
    }

    /// The recording goes on without a live session; `stop` re-delivers it.
    fn went_offline(&mut self, reason: &str) {
        if matches!(self.live, Live::Off) {
            return;
        }
        info!("{} goes on offline: {reason}", self.id);
        self.end_session();
        self.live = Live::Off;
    }

    fn redeliver(&mut self, t: &Timing) {
        self.end_session();
        self.live = Live::Off;
        self.phase = Phase::Finishing;
        self.progress = None;
        self.model_loading = false;
        let now = Instant::now();
        self.window = Some(Window {
            end: now + t.redeliver_window,
            next_attempt: now,
            backoff: t.first_backoff,
            unreachable: false,
        });
    }

    fn fail(&mut self, message: String) {
        warn!("{} failed: {message}", self.id);
        self.end_session();
        self.window = None;
        self.phase = Phase::Failed(message);
    }

    fn state(&self, now: Instant) -> (Dictation, Option<String>, Option<u32>) {
        match &self.phase {
            Phase::Recording => {
                let offline = match self.live {
                    Live::Waiting { offline_at, offline } => offline || now >= offline_at,
                    Live::Streaming { .. } => false,
                    Live::Off => true,
                };
                let left = self.limit.saturating_sub(self.audio.len() as u64);
                let seconds_left =
                    (left <= LIMIT_WARNING_S * SAMPLES_PER_S).then(|| left.div_ceil(SAMPLES_PER_S) as u32);
                (Dictation::Recording, offline.then(|| OFFLINE_RECORDING.into()), seconds_left)
            }
            Phase::Failed(message) => (Dictation::Failed, Some(message.clone()), None),
            Phase::Finishing => {
                let dictation = match (&self.session, &self.window) {
                    (None, Some(window)) if window.unreachable => Dictation::WaitingForServer,
                    (None, _) => Dictation::Sending,
                    (Some(session), _) => match self.progress {
                        Some(ProgressState::Loading) => Dictation::LoadingModel,
                        Some(ProgressState::Queued | ProgressState::Transcribing | ProgressState::Retrying) => {
                            Dictation::Transcribing
                        }
                        None | Some(ProgressState::Recording) if session.upload => Dictation::Sending,
                        None | Some(ProgressState::Recording) if self.model_loading => Dictation::LoadingModel,
                        None | Some(ProgressState::Recording) => Dictation::Transcribing,
                    },
                };
                (dictation, None, None)
            }
        }
    }
}

/// What the `report` for a final needs once the app says it inserted the text.
struct Delivered {
    id: SessionId,
    history_id: HistoryId,
    t_start: Option<Instant>,
    t_first_audio: Option<Instant>,
    t_stop: Option<Instant>,
    t_final: Instant,
}

struct Actor {
    data_dir: PathBuf,
    device_name: String,
    t: Timing,
    events: std_mpsc::Sender<Event>,
    tx: mpsc::UnboundedSender<Internal>,

    config: Config,
    /// `None` when unconfigured, `Err` with what to tell the user when the address doesn't parse.
    endpoint: Option<Result<Endpoint, String>>,
    /// The latest attempt's, until one succeeds.
    failure: Option<Failure>,
    client: Option<Client>,
    epoch: u64,
    connected_at: Instant,
    /// Kept from the latest connection while there is none.
    round_trip: Duration,
    server_version: Option<String>,
    max_recording_s: u32,
    refused_token: Option<Token>,
    attempt: Option<u64>,
    attempts: u64,
    seq: u64,

    /// One dictation at a time: `start` waits for `idle`.
    job: Option<Job>,
    awaiting_insert: VecDeque<Delivered>,
    published: Option<State>,
}

impl Actor {
    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Conn { attempt, result, fresh } => self.on_conn(attempt, result, fresh),
            Internal::Learned(Learned::Paired { server, paired }) => {
                if self.endpoint() != Some(&server) {
                    return;
                }
                self.config.device_id = Some(paired.device_id);
                self.config.token = Some(paired.token);
                self.refused_token = None;
                self.save_config();
            }
            Internal::Learned(Learned::TokenRefused(token)) => self.refused_token = Some(token),
            Internal::Closed { epoch, reason } => self.on_closed(epoch, reason),
            Internal::Session { seq, event } => self.on_session(seq, event),
            Internal::History(result) => self.answer_history(result),
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Configure { server } => self.configure(&server),
            Command::Pair { code } => self.pair(code),
            Command::Warm => self.warm(),
            Command::Start => self.start(),
            Command::Stop => self.stop(),
            Command::Cancel => self.cancel(),
            Command::Retry => self.retry(),
            Command::Discard => {
                if let Some(job) = self.job.take_if(|job| matches!(job.phase, Phase::Failed(_))) {
                    info!("discarded {}", job.id);
                }
            }
            Command::Inserted { session_id } => self.inserted(session_id),
            Command::History => self.history(),
        }
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref().and_then(|endpoint| endpoint.as_ref().ok())
    }

    fn note_bad_address(&mut self) {
        if let Some(Err(message)) = &self.endpoint {
            self.failure = Some(Failure::offline(message.clone()));
        }
    }

    fn save_config(&self) {
        if let Err(err) = self.config.save(&self.data_dir) {
            warn!("cannot save the configuration in {}: {err}", self.data_dir.display());
        }
    }

    fn configure(&mut self, server: &str) {
        let server = normalise(server);
        if server != self.config.server {
            info!("server: {}", server.as_deref().unwrap_or("none"));
            self.drop_client();
            self.config = Config { server, device_id: None, token: None };
            self.save_config();
            self.endpoint = parse(self.config.server.as_deref());
            self.server_version = None;
            self.refused_token = None;
            self.max_recording_s = DEFAULT_MAX_RECORDING_S;
        }
        // Asked for: whatever went wrong before is worth another try, and the UI says so.
        self.failure = None;
        self.note_bad_address();
        self.warm();
    }

    /// A running attempt's result is ignored when it comes.
    fn drop_client(&mut self) {
        if let Some(client) = self.client.take() {
            client.close();
        }
        self.epoch += 1;
        self.attempt = None;
    }

    fn client_alive(&self) -> bool {
        self.client.as_ref().is_some_and(|client| !client.is_closed())
    }

    fn warm(&mut self) {
        let kind = if self.client_alive() { AttemptKind::Check } else { AttemptKind::Connect };
        self.connect(kind, self.t.connect_budget);
    }

    fn pong_budget(&self) -> Duration {
        self.t.pong.max(self.round_trip.saturating_mul(3).min(MAX_PONG))
    }

    /// Room for the pong wait and a reconnect: TCP, the WebSocket upgrade, `hello`/`welcome`, and
    /// a round trip to spare.
    fn check_budget(&self, floor: Duration) -> Duration {
        floor.max((self.pong_budget() + self.round_trip.saturating_mul(4)).min(MAX_CHECK))
    }

    fn next_attempt(&mut self) -> u64 {
        self.attempts += 1;
        self.attempt = Some(self.attempts);
        self.attempts
    }

    /// No reconnect timers: attempts are made at start-up, on `configure`, `warm`, `pair` and
    /// `start`, once after a connection closes, and in a re-delivery window.
    fn connect(&mut self, kind: AttemptKind, budget: Duration) {
        if self.attempt.is_some() {
            return;
        }
        let Some(endpoint) = self.endpoint().cloned() else { return };
        let attempt = self.next_attempt();
        let token = self.config.token.clone();
        let refused_before = token.is_some() && token == self.refused_token;
        let current = self.client.clone().filter(|client| kind == AttemptKind::Check && !client.is_closed());
        let pong = self.pong_budget().min(budget);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let learned = |learned: Learned| {
                let _ = tx.send(Internal::Learned(learned));
            };
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
                (connect::connect_or_pair(&endpoint, token, refused_before, &learned).await, true)
            };
            let (result, fresh) = timeout(budget, work).await.unwrap_or_else(|_| (Err(Failure::no_answer()), true));
            let _ = tx.send(Internal::Conn { attempt, result, fresh });
        });
    }

    fn pair(&mut self, code: String) {
        let Some(endpoint) = self.endpoint().cloned() else {
            info!("pair ignored: no server");
            return;
        };
        // Replaces any attempt in progress: the user asked for this one.
        let attempt = self.next_attempt();
        self.failure = None;
        let (name, budget, tx) = (self.device_name.clone(), self.t.connect_budget, self.tx.clone());
        tokio::spawn(async move {
            let learned = |learned: Learned| {
                let _ = tx.send(Internal::Learned(learned));
            };
            let work = connect::pair_with_code(&endpoint, &code, &name, &learned);
            let result = timeout(budget, work).await.unwrap_or_else(|_| Err(Failure::no_answer()));
            let _ = tx.send(Internal::Conn { attempt, result, fresh: true });
        });
    }

    fn on_conn(&mut self, attempt: u64, result: Result<Client, Failure>, fresh: bool) {
        if self.attempt != Some(attempt) {
            if let (Ok(client), true) = (&result, fresh) {
                client.close();
            }
            return;
        }
        self.attempt = None;
        match result {
            Ok(client) => {
                self.round_trip = client.round_trip();
                self.failure = None;
                self.refused_token = None;
                if fresh {
                    self.adopt(client);
                }
            }
            Err(failure) => {
                info!("cannot connect: {}", failure.message);
                if fresh && let Some(old) = self.client.take() {
                    old.close();
                }
                self.attempt_failed(&failure);
                self.failure = Some(failure);
            }
        }
    }

    fn adopt(&mut self, client: Client) {
        if let Some(old) = self.client.take() {
            old.close();
        }
        self.epoch += 1;
        let epoch = self.epoch;
        let welcome = client.welcome();
        info!(
            "connected to {} (psst {}) as {} ({} ms round trip)",
            client.endpoint(),
            welcome.server_version,
            welcome.device,
            self.round_trip.as_millis()
        );
        self.server_version = Some(welcome.server_version.clone());
        self.max_recording_s = welcome.limits.max_recording_s;
        let (watcher, tx) = (client.clone(), self.tx.clone());
        tokio::spawn(async move {
            let reason = watcher.closed().await;
            let _ = tx.send(Internal::Closed { epoch, reason });
        });
        self.client = Some(client);
        self.connected_at = Instant::now();
    }

    fn attempt_failed(&mut self, failure: &Failure) {
        let Some(job) = &mut self.job else { return };
        match (&job.phase, &mut job.live, &mut job.window) {
            (Phase::Recording, Live::Waiting { offline, .. }, _) => *offline = true,
            (Phase::Finishing, _, Some(window)) if job.session.is_none() => {
                window.unreachable = true;
                // Trying again can't help until the user pairs the phone or updates a side.
                if failure.link != Link::Offline {
                    job.fail(undeliverable(Some(failure.link)));
                }
            }
            _ => {}
        }
    }

    fn on_closed(&mut self, epoch: u64, reason: String) {
        if epoch != self.epoch {
            return;
        }
        info!("connection closed: {reason}");
        self.client = None;
        self.failure = Some(Failure::offline(CONNECTION_DROPPED));
        if self.connected_at.elapsed() >= RECONNECT_AFTER {
            self.connect(AttemptKind::Connect, self.t.connect_budget);
        }
    }

    fn start(&mut self) {
        if self.job.is_some() {
            debug!("start ignored: a dictation is in progress");
            return;
        }
        if self.endpoint().is_none() {
            info!("start ignored: no server");
            return;
        }
        let id = SessionId::new_v4();
        let now = Instant::now();
        self.job = Some(Job {
            id,
            audio: Arc::default(),
            limit: limit(self.max_recording_s),
            phase: Phase::Recording,
            live: Live::Waiting { offline_at: now + self.check_budget(self.t.start_budget), offline: false },
            session: None,
            ending: None,
            model_loading: false,
            progress: None,
            window: None,
            t_start: Some(now),
            t_first_audio: None,
            t_stop: None,
        });
        info!("recording {id}");
        self.warm();
    }

    fn on_audio(&mut self, samples: &[i16]) {
        let Some(job) = self.job.as_mut().filter(|job| job.phase == Phase::Recording) else { return };
        job.t_first_audio.get_or_insert_with(Instant::now);
        let room = job.limit.saturating_sub(job.audio.len() as u64) as usize;
        Arc::make_mut(&mut job.audio).extend_from_slice(&samples[..samples.len().min(room)]);
        let streamed = job.stream(false);
        let full = job.audio.len() as u64 >= job.limit;
        if let Err((reason, congested)) = streamed {
            job.went_offline(&reason);
            if congested && let Some(client) = &self.client {
                client.close();
            }
        }
        if full {
            info!("{} reached the recording limit", job.id);
            self.stop();
        }
    }

    fn stop(&mut self) {
        let Some(job) = self.job.as_mut().filter(|job| job.phase == Phase::Recording) else { return };
        let samples = job.audio.len() as u64;
        info!("stopped {} after {} ms", job.id, samples * 1000 / SAMPLES_PER_S);
        job.t_stop = Some(Instant::now());
        job.phase = Phase::Finishing;
        if matches!(job.live, Live::Streaming { .. }) {
            match job.stream(true) {
                Ok(()) => {
                    if let Some(session) = &job.session {
                        let _ = session.ctl.send(Ctl::Stop(samples));
                    }
                    return;
                }
                Err((reason, congested)) => {
                    info!("{}: {reason}; re-delivering it", job.id);
                    if congested && let Some(client) = &self.client {
                        client.close();
                    }
                }
            }
        }
        if samples == 0 {
            let id = job.id;
            self.job = None;
            info!("{id}: nothing recorded");
            let _ = self.events.send(Event::Final { session_id: id, text: String::new() });
            return;
        }
        job.redeliver(&self.t);
    }

    fn cancel(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.end_session();
            info!("cancelled {}", job.id);
        }
    }

    fn retry(&mut self) {
        let Some(job) = self.job.as_mut().filter(|job| matches!(job.phase, Phase::Failed(_))) else { return };
        info!("retrying {}", job.id);
        // A retry has no toggle or first frame to time.
        job.t_start = None;
        job.t_first_audio = None;
        job.t_stop = Some(Instant::now());
        job.redeliver(&self.t);
    }

    fn on_session(&mut self, seq: u64, event: SessionEvent) {
        let Some(job) = &mut self.job else { return };
        if job.ending == Some(seq) {
            if event.is_terminal() {
                job.ending = None;
            }
            return;
        }
        if job.session.as_ref().is_none_or(|session| session.seq != seq) {
            return;
        }
        if event.is_terminal() {
            job.session = None;
        }
        let live = job.window.is_none();
        match event {
            SessionEvent::Ready { need_audio, model_state, .. } => {
                job.model_loading = model_state == ReadyModelState::Loading;
                // An upload the server already has: it is transcribing, or done.
                if !need_audio {
                    job.progress.get_or_insert(ProgressState::Transcribing);
                }
            }
            SessionEvent::Progress { state, .. } => job.progress = Some(state),
            SessionEvent::Final(msg) => self.deliver(msg),
            SessionEvent::Rejected(err) | SessionEvent::Error(err) if job.phase == Phase::Recording => {
                job.went_offline(&err.message);
            }
            SessionEvent::Lost(reason) if job.phase == Phase::Recording => job.went_offline(&reason.to_string()),
            SessionEvent::Cancelled if job.phase == Phase::Recording => job.went_offline("the server cancelled it"),
            SessionEvent::Lost(reason) => {
                info!("{}: {reason}", job.id);
                if live {
                    job.redeliver(&self.t);
                }
            }
            SessionEvent::Cancelled => {
                info!("{}: the server cancelled it", job.id);
                if live {
                    job.redeliver(&self.t);
                }
            }
            SessionEvent::Rejected(err) if live => {
                info!("{}: the server refused it ({}); re-delivering it", job.id, err.message);
                job.redeliver(&self.t);
            }
            SessionEvent::Error(err) if live && err.code == ErrorCode::BadAudio => {
                info!("{}: the server got different audio ({}); re-delivering it", job.id, err.message);
                job.redeliver(&self.t);
            }
            SessionEvent::Rejected(err) | SessionEvent::Error(err) => {
                let reason = connect::sentence(&err.message);
                job.fail(format!("The server could not transcribe it ({reason}): retry, or discard the recording."));
            }
        }
    }

    fn deliver(&mut self, msg: Box<FinalMsg>) {
        let Some(job) = self.job.take() else { return };
        let text = if msg.r#final.text.trim().is_empty() { String::new() } else { msg.r#final.text };
        info!("final for {}: {} characters", job.id, text.chars().count());
        let empty = text.is_empty();
        let _ = self.events.send(Event::Final { session_id: job.id, text });
        let delivered = Delivered {
            id: job.id,
            history_id: msg.history_id,
            t_start: job.t_start,
            t_first_audio: job.t_first_audio,
            t_stop: job.t_stop,
            t_final: Instant::now(),
        };
        if empty {
            // Nothing to insert, so the app won't say it did.
            self.report(&delivered, None);
            return;
        }
        if self.awaiting_insert.len() == AWAITING_INSERT {
            self.awaiting_insert.pop_front();
        }
        self.awaiting_insert.push_back(delivered);
    }

    fn inserted(&mut self, id: SessionId) {
        let Some(pos) = self.awaiting_insert.iter().position(|d| d.id == id) else {
            debug!("inserted {id}, which has no final waiting");
            return;
        };
        let delivered = self.awaiting_insert.remove(pos).expect("position is in range");
        self.report(&delivered, Some(Instant::now()));
    }

    fn report(&self, d: &Delivered, dispatched: Option<Instant>) {
        let ms = |from: Option<Instant>, to: Option<Instant>| match (from, to) {
            (Some(from), Some(to)) => Some(to.saturating_duration_since(from).as_millis() as u32),
            _ => None,
        };
        let timings = ClientTimings {
            toggle_to_overlay_ms: None,
            toggle_to_first_frame_ms: ms(d.t_start, d.t_first_audio),
            stop_to_final_ms: ms(d.t_stop, Some(d.t_final)),
            final_to_dispatch_ms: ms(Some(d.t_final), dispatched),
            stop_to_dispatch_ms: ms(d.t_stop, dispatched),
        };
        match self.client.as_ref().filter(|client| !client.is_closed()) {
            Some(client) => client.report(d.id, d.history_id, timings),
            None => debug!("no connection to report {} on", d.id),
        }
    }

    fn history(&mut self) {
        let (endpoint, device) = match (&self.endpoint, &self.config.device_id) {
            (None, _) => return self.answer_history(Err(NO_SERVER.into())),
            (Some(Err(message)), _) => return self.answer_history(Err(message.clone())),
            (Some(Ok(_)), None) => return self.answer_history(Err(NOT_PAIRED.into())),
            (Some(Ok(endpoint)), Some(device)) => (endpoint.clone(), device.clone()),
        };
        let (token, budget, tx) = (self.config.token.clone(), self.t.history_budget, self.tx.clone());
        tokio::spawn(async move {
            let fetch = history::fetch(endpoint, token, device);
            let result = timeout(budget, fetch).await.unwrap_or_else(|_| Err(Failure::no_answer().message));
            let _ = tx.send(Internal::History(result));
        });
    }

    fn answer_history(&self, result: Result<Vec<HistoryItem>, String>) {
        let (items, error) = match result {
            Ok(items) => (items, None),
            Err(error) => {
                info!("cannot list recent dictations: {error}");
                (Vec::new(), Some(error))
            }
        };
        let _ = self.events.send(Event::History { items, error });
    }

    /// Starts the sessions and attempts the dictation is waiting for.
    fn pump(&mut self) {
        if self.client.as_ref().is_some_and(Client::is_closed) {
            self.client = None;
        }
        let now = Instant::now();
        let usable = self.attempt.is_none() && self.client.is_some();
        let Some(job) = &mut self.job else { return };
        match (&job.phase, &job.live, &mut job.window) {
            (Phase::Recording, Live::Waiting { .. }, _) if usable => self.start_live(),
            (Phase::Finishing, _, Some(_)) if job.session.is_none() && usable => {
                if job.ending.is_none() {
                    self.start_upload();
                }
            }
            (Phase::Finishing, _, Some(window)) if job.session.is_none() && self.attempt.is_none() => {
                if now >= window.end {
                    let link = self.failure.as_ref().map(|failure| failure.link);
                    job.fail(undeliverable(link));
                } else if now >= window.next_attempt {
                    let budget = self.t.attempt_budget.min(window.end - now).max(Duration::from_millis(50));
                    window.next_attempt = now + window.backoff;
                    window.backoff = (window.backoff * 2).min(MAX_BACKOFF);
                    self.connect(AttemptKind::Connect, budget);
                }
            }
            _ => {}
        }
    }

    fn start_live(&mut self) {
        let (Some(client), Some(job)) = (&mut self.client, &mut self.job) else { return };
        if job.audio.len() / FRAME_SAMPLES >= FRAME_BUFFER {
            return job.went_offline("too much audio waiting for the server");
        }
        self.seq += 1;
        job.limit = limit(client.welcome().limits.max_recording_s);
        let session = client.start(StartOpts {
            session_id: job.id,
            profile: None,
            mode: Mode::Live,
            resume: false,
            eval: None,
            codec: codec(),
        });
        job.session = Some(spawn_session(self.seq, session, false, self.tx.clone()));
        job.live = Live::Streaming { sent: 0 };
        if let Err((reason, _)) = job.stream(false) {
            job.went_offline(&reason);
        }
    }

    fn start_upload(&mut self) {
        let (Some(client), Some(job)) = (&mut self.client, &mut self.job) else { return };
        info!("re-delivering {}", job.id);
        self.seq += 1;
        let session = client.start(StartOpts {
            session_id: job.id,
            profile: None,
            mode: Mode::Upload,
            resume: true,
            eval: None,
            codec: codec(),
        });
        let ctl = spawn_session(self.seq, session, true, self.tx.clone());
        let (frames, stop, audio) = (ctl.frames.clone(), ctl.ctl.clone(), job.audio.clone());
        // Ends early when the server needs no audio, or the session ends.
        tokio::spawn(async move {
            for index in 0..audio.len().div_ceil(FRAME_SAMPLES) {
                if frames.send(frame(&audio, index)).await.is_err() {
                    return;
                }
            }
            let _ = stop.send(Ctl::Stop(audio.len() as u64));
        });
        job.session = Some(ctl);
        job.progress = None;
        job.model_loading = false;
    }

    fn next_deadline(&self) -> Option<Instant> {
        let job = self.job.as_ref()?;
        match (&job.phase, &job.live, &job.window) {
            (Phase::Recording, Live::Waiting { offline_at, offline: false }, _) if Instant::now() < *offline_at => {
                Some(*offline_at)
            }
            (Phase::Finishing, _, Some(window)) if job.session.is_none() && self.attempt.is_none() => {
                Some(window.next_attempt.min(window.end))
            }
            _ => None,
        }
    }

    fn link(&self) -> (Link, Option<String>) {
        if self.config.server.is_none() {
            return (Link::Unconfigured, None);
        }
        if self.client_alive() {
            return (Link::Online, None);
        }
        match (&self.failure, self.attempt) {
            // Retried in the background (a `warm`), these stay up rather than flicker, so that a
            // pairing code being typed doesn't lose its field.
            (Some(failure), Some(_)) if failure.link != Link::Offline => (failure.link, Some(failure.message.clone())),
            (_, Some(_)) => (Link::Connecting, None),
            (Some(failure), None) => (failure.link, Some(failure.message.clone())),
            (None, None) => (Link::Offline, Some(NOT_CONNECTED.into())),
        }
    }

    fn publish(&mut self) {
        let (link, link_message) = self.link();
        let (dictation, dictation_message, seconds_left) = match &self.job {
            Some(job) => job.state(Instant::now()),
            None => (Dictation::Idle, None, None),
        };
        let state = State {
            server: self.config.server.clone(),
            server_version: self.server_version.clone(),
            link,
            link_message,
            dictation,
            dictation_message,
            seconds_left,
        };
        if self.published.as_ref() != Some(&state) {
            self.published = Some(state.clone());
            let _ = self.events.send(Event::State { state });
        }
    }
}

fn undeliverable(link: Option<Link>) -> String {
    match link {
        Some(Link::NeedsPairing) => "The server no longer accepts this phone: pair it again, then retry.",
        Some(Link::UpdateApp) => "The server needs a newer app: update psst, then retry.",
        Some(Link::UpdateServer) => "The server is too old for this app: update it, then retry.",
        _ => "Could not reach the server: retry, or discard the recording.",
    }
    .into()
}
