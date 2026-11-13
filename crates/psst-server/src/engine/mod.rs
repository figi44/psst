//! The engine supervisor: one engine process for the one model, a first-in
//! first-out queue of transcriptions, and every budget, retry and restart decision.
//!
//! The supervisor is an actor: [`Supervisor`] is a cheap handle that sends it commands, and the
//! model's state is published on a watch channel. All engine I/O, timers and state live in the
//! actor task, so no locks are held across them.

mod process;

use std::{
    collections::{HashMap, VecDeque},
    mem,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use psst_config::ModelOptions;
use psst_engine_proto::{
    Bias, EngineErrorCode, FromEngine, Load, LoadPhase, PROTOCOL_VERSION, StreamRef, ToEngine, Transcribe,
    TranscribeResult,
};
use psst_proto::{
    ErrorCode,
    api::{DownloadProgress, ModelState, ModelStatus},
};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

use self::process::{EngineProcess, ProcEvent};

/// A load fails after this long without `progress` or `loaded` (and a start without `hello`).
pub const LOAD_SILENCE: Duration = Duration::from_secs(60);
/// Every attempt gets this much plus the recording's length.
pub const ATTEMPT_BASE: Duration = Duration::from_secs(5);
/// A cancelled running request gets this long to finish before the engine is killed.
pub const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// After `shutdown`, the engine gets this long before `SIGTERM`…
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// …and this long after `SIGTERM` before `SIGKILL`.
pub const TERM_GRACE: Duration = Duration::from_secs(2);
/// Attempts per request.
pub const MAX_ATTEMPTS: u32 = 2;
/// Crashes in a row that mark the model failed.
pub const CRASH_LIMIT: u32 = 3;
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// What the supervisor needs from `server.toml` and the command line.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub engine: String,
    pub command: Vec<String>,
    pub model: String,
    pub revision: Option<String>,
    pub device: String,
    pub compute_type: String,
    pub options: ModelOptions,
    /// `HF_HOME` for the engine.
    pub cache_dir: PathBuf,
    pub max_recording_s: u32,
    pub load_deadline: Duration,
}

impl EngineConfig {
    pub fn from_server(cfg: &psst_config::ServerConfig, cache_dir: PathBuf) -> Self {
        let model = &cfg.model;
        Self {
            engine: model.engine.clone(),
            command: model.command.clone(),
            model: model.model.clone(),
            revision: model.revision.clone(),
            device: model.device.clone(),
            compute_type: model.compute_type.clone(),
            options: model.options,
            cache_dir,
            max_recording_s: cfg.limits.max_recording_s,
            load_deadline: Duration::from_secs(cfg.load_deadline_s.into()),
        }
    }
}

pub type JobId = u64;

/// Where a request is, for `progress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Queued,
    Transcribing,
    /// Waiting for another attempt after a crash, an `oom` or a timeout.
    Retrying,
}

/// A request's phase, shared with the session that made it.
#[derive(Debug, Default)]
pub struct JobStatus(AtomicU8);

impl JobStatus {
    fn set(&self, phase: JobPhase) {
        self.0.store(phase as u8, Ordering::Relaxed);
    }

    pub fn get(&self) -> JobPhase {
        match self.0.load(Ordering::Relaxed) {
            1 => JobPhase::Transcribing,
            2 => JobPhase::Retrying,
            _ => JobPhase::Queued,
        }
    }
}

/// One transcription to run.
#[derive(Debug)]
pub struct JobRequest {
    pub language: String,
    pub bias: Bias,
    /// The recording as a frame payload: `f32` little-endian at 16 kHz.
    pub payload: Vec<u8>,
    pub audio_ms: u32,
    /// The recording's stream, if it was sent to the engine as it came in: the request then
    /// closes the stream, and `payload` is only for falling back to `transcribe`.
    pub stream: Option<Stream>,
}

/// A live recording sent to the engine while it comes in (the "Streams" section of
/// `psst_engine_proto`'s docs), from [`Supervisor::open_stream`]. Dropping it discards the stream;
/// a [`JobRequest`] closes it.
#[derive(Debug)]
pub struct Stream {
    id: JobId,
    commands: mpsc::UnboundedSender<Command>,
    submitted: bool,
}

impl Stream {
    /// The recording's next samples, `pcm_s16le`.
    pub fn append(&self, pcm: &[u8]) {
        let _ = self.commands.send(Command::Append(self.id, pcm_to_payload(pcm)));
    }

    fn submit(mut self) -> JobId {
        self.submitted = true;
        self.id
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.submitted {
            let _ = self.commands.send(Command::Discard(self.id));
        }
    }
}

/// `pcm_s16le` as an engine audio payload: `f32` little-endian.
pub fn pcm_to_payload(pcm: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(pcm.len() * 2);
    for pair in pcm.as_chunks::<2>().0 {
        payload.extend_from_slice(&(f32::from(i16::from_le_bytes(*pair)) / 32768.0).to_le_bytes());
    }
    payload
}

/// A submitted request.
#[derive(Debug)]
pub struct JobHandle {
    pub id: JobId,
    pub status: Arc<JobStatus>,
    pub outcome: oneshot::Receiver<JobOutcome>,
}

#[derive(Debug)]
pub enum JobOutcome {
    Done(Box<JobDone>),
    Failed { code: ErrorCode, message: String, detail: Option<Value> },
    Cancelled,
}

#[derive(Debug)]
pub struct JobDone {
    pub result: TranscribeResult,
    pub queue_ms: u32,
    pub load_wait_ms: u32,
    pub engine_ms: u32,
    pub attempts: u32,
}

/// Handle to the supervisor actor.
#[derive(Debug, Clone)]
pub struct Supervisor {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<ModelStatus>,
    next_job: Arc<AtomicU64>,
}

#[derive(Debug)]
enum Command {
    Load(oneshot::Sender<ModelStatus>),
    Unload(oneshot::Sender<ModelStatus>),
    Submit(Job),
    Cancel(JobId),
    Open { id: JobId, language: String, bias: Bias },
    Append(JobId, Vec<u8>),
    Discard(JobId),
    Resume,
    Shutdown(oneshot::Sender<()>),
}

impl Supervisor {
    /// Starts the actor. With `load`, it starts the engine and loads the model at once;
    /// otherwise the model stays `unloaded` until [`Supervisor::load`].
    pub fn start(cfg: EngineConfig, load: bool) -> Supervisor {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let target = if load { Target::Loaded } else { Target::Unloaded };
        let initial = ModelStatus {
            name: cfg.model.clone(),
            state: if load { ModelState::Loading } else { ModelState::Unloaded },
            revision: None,
            progress: None,
            error: None,
        };
        let (status_tx, status) = watch::channel(initial);
        let actor = Actor {
            cfg,
            commands: command_rx,
            events,
            event_rx,
            status: status_tx,
            target,
            phase: Phase::Idle,
            queue: VecDeque::new(),
            crash_streak: 0,
            generation: 0,
            download: None,
            revision: None,
            unload_waiters: Vec::new(),
            shutdown_waiters: Vec::new(),
            shutting_down: false,
            streams_supported: false,
            streams: HashMap::new(),
        };
        tokio::spawn(actor.run());
        Supervisor { commands, status, next_job: Arc::new(AtomicU64::new(1)) }
    }

    pub fn status(&self) -> ModelStatus {
        self.status.borrow().clone()
    }

    /// Clears an unload or a failure and loads the model; returns the state right after.
    pub async fn load(&self) -> ModelStatus {
        let (reply, answer) = oneshot::channel();
        let _ = self.commands.send(Command::Load(reply));
        answer.await.unwrap_or_else(|_| self.status())
    }

    /// Unloads the model on purpose and returns once the engine has exited.
    pub async fn unload(&self) -> ModelStatus {
        let (reply, answer) = oneshot::channel();
        let _ = self.commands.send(Command::Unload(reply));
        answer.await.unwrap_or_else(|_| self.status())
    }

    /// Starts sending a live recording to the engine as it comes in. Only an engine that takes
    /// streams gets it, and only while the model is loaded; otherwise the request that closes it
    /// sends the whole recording as `transcribe`.
    pub fn open_stream(&self, language: String, bias: Bias) -> Stream {
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let _ = self.commands.send(Command::Open { id, language, bias });
        Stream { id, commands: self.commands.clone(), submitted: false }
    }

    pub fn submit(&self, request: JobRequest) -> JobHandle {
        let streamed = request.stream.is_some();
        let id = match request.stream {
            Some(stream) => stream.submit(),
            None => self.next_job.fetch_add(1, Ordering::Relaxed),
        };
        let status = Arc::new(JobStatus::default());
        let (done, outcome) = oneshot::channel();
        let job = Job {
            id,
            language: request.language,
            bias: request.bias,
            payload: Arc::new(request.payload),
            audio_ms: request.audio_ms,
            streamed,
            status: status.clone(),
            done: Some(done),
            enqueued: Instant::now(),
            first_sent: None,
            attempts: 0,
            oom_retry: false,
            engine_time: Duration::ZERO,
            waited: Duration::ZERO,
            waiting_since: None,
        };
        if let Err(mpsc::error::SendError(Command::Submit(mut job))) = self.commands.send(Command::Submit(job)) {
            job.fail(ErrorCode::Internal, "the server is shutting down", None);
        }
        JobHandle { id, status, outcome }
    }

    /// Cancels a request: a queued one is removed, a running one gets [`CANCEL_GRACE`] before the
    /// engine is killed and restarted. Its outcome is [`JobOutcome::Cancelled`].
    pub fn cancel(&self, id: JobId) {
        let _ = self.commands.send(Command::Cancel(id));
    }

    /// The machine woke up: restart a loaded engine.
    pub fn resume(&self) {
        let _ = self.commands.send(Command::Resume);
    }

    /// Fails waiting requests and stops the engine.
    pub async fn shutdown(&self) {
        let (reply, answer) = oneshot::channel();
        if self.commands.send(Command::Shutdown(reply)).is_ok() {
            let _ = answer.await;
        }
    }
}

#[derive(Debug)]
struct Job {
    id: JobId,
    language: String,
    bias: Bias,
    payload: Arc<Vec<u8>>,
    audio_ms: u32,
    /// Its id is a stream's, which it closes if the stream is still open in this engine.
    streamed: bool,
    status: Arc<JobStatus>,
    done: Option<oneshot::Sender<JobOutcome>>,
    enqueued: Instant,
    first_sent: Option<Instant>,
    attempts: u32,
    oom_retry: bool,
    engine_time: Duration,
    /// Time spent waiting for the model, for the load deadline.
    waited: Duration,
    waiting_since: Option<Instant>,
}

impl Job {
    fn finish(&mut self, outcome: JobOutcome) {
        if let Some(done) = self.done.take() {
            let _ = done.send(outcome);
        }
    }

    fn fail(&mut self, code: ErrorCode, message: impl Into<String>, detail: Option<Value>) {
        self.finish(JobOutcome::Failed { code, message: message.into(), detail });
    }

    fn cancelled(&self) -> bool {
        self.done.is_none()
    }

    fn wait_total(&self, now: Instant) -> Duration {
        self.waited + self.waiting_since.map_or(Duration::ZERO, |since| now - since)
    }
}

/// What the model should be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Loaded,
    /// On purpose; nothing reloads it but [`Supervisor::load`].
    Unloaded,
    Failed(String),
}

#[derive(Debug)]
enum Phase {
    /// No process.
    Idle,
    /// No process; start one at this instant.
    Backoff(Instant),
    /// Waiting for `hello`.
    Starting {
        proc: EngineProcess,
        silent_until: Instant,
    },
    Loading {
        proc: EngineProcess,
        silent_until: Instant,
    },
    Ready {
        proc: EngineProcess,
    },
    Busy {
        proc: EngineProcess,
        run: Box<Run>,
    },
    /// Ending the process on purpose; the next process starts `restart_after` its exit.
    Stopping {
        proc: EngineProcess,
        stage: Stop,
        restart_after: Duration,
    },
}

#[derive(Debug)]
struct Run {
    job: Job,
    sent: Instant,
    deadline: Instant,
    /// Set when the request was cancelled: the engine is killed at this instant.
    kill_at: Option<Instant>,
    /// Sent as `close` rather than `transcribe`.
    closes_stream: bool,
}

#[derive(Debug, Clone, Copy)]
enum Stop {
    /// `shutdown` sent; `SIGTERM` at this instant.
    Shutdown(Instant),
    /// `SIGTERM` sent; `SIGKILL` at this instant.
    Term(Instant),
    Kill,
}

impl Phase {
    fn proc(&self) -> Option<&EngineProcess> {
        match self {
            Phase::Idle | Phase::Backoff(_) => None,
            Phase::Starting { proc, .. }
            | Phase::Loading { proc, .. }
            | Phase::Ready { proc }
            | Phase::Busy { proc, .. }
            | Phase::Stopping { proc, .. } => Some(proc),
        }
    }
}

struct Actor {
    cfg: EngineConfig,
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<(u64, ProcEvent)>,
    event_rx: mpsc::UnboundedReceiver<(u64, ProcEvent)>,
    status: watch::Sender<ModelStatus>,
    target: Target,
    phase: Phase,
    queue: VecDeque<Job>,
    crash_streak: u32,
    generation: u64,
    download: Option<DownloadProgress>,
    revision: Option<String>,
    unload_waiters: Vec<oneshot::Sender<ModelStatus>>,
    shutdown_waiters: Vec<oneshot::Sender<()>>,
    shutting_down: bool,
    /// Whether the running engine takes streams.
    streams_supported: bool,
    /// The streams open in an engine, with that engine's generation.
    streams: HashMap<JobId, u64>,
}

fn ms(duration: Duration) -> u32 {
    duration.as_millis().try_into().unwrap_or(u32::MAX)
}

fn backoff(streak: u32) -> Duration {
    let factor = 1u32 << streak.saturating_sub(1).min(5);
    (BACKOFF_FIRST * factor).min(BACKOFF_MAX)
}

impl Actor {
    async fn run(mut self) {
        loop {
            let now = Instant::now();
            self.on_timers(now);
            self.drive(now);
            self.update_wait_clocks(now);
            self.publish();
            if self.shutting_down && self.phase.proc().is_none() {
                for waiter in self.shutdown_waiters.drain(..) {
                    let _ = waiter.send(());
                }
                return;
            }
            let wake = self.next_wake();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command),
                    None => self.begin_shutdown(Instant::now()),
                },
                Some((generation, event)) = self.event_rx.recv() => self.on_event(generation, event),
                _ = sleep_until(wake) => {}
            }
        }
    }

    fn on_command(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::Load(reply) => {
                if self.target != Target::Loaded {
                    tracing::info!("loading the model");
                    self.target = Target::Loaded;
                    self.crash_streak = 0;
                    self.download = None;
                    if let Phase::Backoff(_) = self.phase {
                        self.phase = Phase::Backoff(now);
                    }
                }
                self.drive(now);
                let _ = reply.send(self.model_status());
            }
            Command::Unload(reply) => {
                tracing::info!("unloading the model");
                self.target = Target::Unloaded;
                self.crash_streak = 0;
                self.fail_waiting(ErrorCode::ModelUnloaded, "the model was unloaded on purpose", None);
                self.stop_engine(now, false);
                self.unload_waiters.push(reply);
            }
            Command::Submit(mut job) => match &self.target {
                _ if self.shutting_down => job.fail(ErrorCode::Internal, "the server is shutting down", None),
                Target::Unloaded => job.fail(ErrorCode::ModelUnloaded, unloaded_message(), None),
                Target::Failed(error) => {
                    let detail = json!({"reason": "failed", "error": error});
                    job.fail(ErrorCode::ModelUnavailable, format!("the model failed: {error}"), Some(detail));
                }
                Target::Loaded => {
                    job.status.set(JobPhase::Queued);
                    self.queue.push_back(job);
                }
            },
            Command::Cancel(id) => {
                if let Some(pos) = self.queue.iter().position(|job| job.id == id) {
                    let mut job = self.queue.remove(pos).expect("position is in range");
                    self.discard_stream(id);
                    job.finish(JobOutcome::Cancelled);
                } else if let Phase::Busy { run, .. } = &mut self.phase
                    && run.job.id == id
                    && run.kill_at.is_none()
                {
                    run.kill_at = Some(now + CANCEL_GRACE);
                    run.job.finish(JobOutcome::Cancelled);
                }
            }
            Command::Open { id, language, bias } => {
                if self.streams_supported
                    && let Phase::Ready { proc } | Phase::Busy { proc, .. } = &self.phase
                {
                    let options = self.cfg.options.to_json();
                    proc.send(&ToEngine::Open(Transcribe { id, language, bias, options }), Arc::default());
                    self.streams.insert(id, proc.generation);
                }
            }
            Command::Append(id, payload) => {
                if let Some(proc) = self.phase.proc()
                    && self.streams.get(&id) == Some(&proc.generation)
                {
                    proc.send(&ToEngine::Append(StreamRef { id }), Arc::new(payload));
                }
            }
            Command::Discard(id) => self.discard_stream(id),
            Command::Resume => {
                if self.target == Target::Loaded && !self.shutting_down {
                    self.restart_after_resume();
                }
            }
            Command::Shutdown(reply) => {
                self.shutdown_waiters.push(reply);
                self.begin_shutdown(now);
            }
        }
    }

    fn on_event(&mut self, generation: u64, event: ProcEvent) {
        if self.phase.proc().map(|proc| proc.generation) != Some(generation) {
            return;
        }
        let now = Instant::now();
        match event {
            ProcEvent::Message(msg) => self.on_message(now, *msg),
            ProcEvent::Corrupt(why) => self.on_crash(now, format!("corrupt stream: {why}"), false, false),
            ProcEvent::Exited(status) => {
                if let Phase::Stopping { restart_after, .. } = self.phase {
                    tracing::info!("engine stopped ({status})");
                    self.phase = self.after_stop(now, restart_after);
                } else {
                    self.on_crash(now, format!("exited with {status}"), true, false);
                }
            }
        }
    }

    fn on_message(&mut self, now: Instant, msg: FromEngine) {
        match (msg, mem::replace(&mut self.phase, Phase::Idle)) {
            (FromEngine::Hello(hello), Phase::Starting { proc, silent_until }) => {
                self.phase = Phase::Starting { proc, silent_until };
                if hello.protocol != PROTOCOL_VERSION {
                    let error = format!(
                        "the engine speaks protocol {}, but the server speaks {PROTOCOL_VERSION}",
                        hello.protocol
                    );
                    self.fail_model(now, error, true);
                } else if self.cfg.max_recording_s > hello.capabilities.max_audio_s {
                    let error = format!(
                        "limits.max_recording_s = {} is above the engine's max_audio_s = {}",
                        self.cfg.max_recording_s, hello.capabilities.max_audio_s
                    );
                    self.fail_model(now, error, true);
                } else {
                    tracing::info!("engine {} {} started; loading {}", hello.engine, hello.version, self.cfg.model);
                    self.streams_supported = hello.capabilities.stream;
                    let Phase::Starting { proc, .. } = mem::replace(&mut self.phase, Phase::Idle) else {
                        unreachable!()
                    };
                    proc.send(
                        &ToEngine::Load(Load {
                            model: self.cfg.model.clone(),
                            revision: self.cfg.revision.clone(),
                            device: self.cfg.device.clone(),
                            compute_type: self.cfg.compute_type.clone(),
                        }),
                        Arc::default(),
                    );
                    self.phase = Phase::Loading { proc, silent_until: now + LOAD_SILENCE };
                }
            }
            (FromEngine::Progress(progress), Phase::Loading { proc, .. }) => {
                self.download = match progress.phase {
                    LoadPhase::Download => {
                        Some(DownloadProgress { bytes: progress.bytes.unwrap_or(0), total: progress.total })
                    }
                    LoadPhase::Load => None,
                };
                self.phase = Phase::Loading { proc, silent_until: now + LOAD_SILENCE };
            }
            (FromEngine::Loaded(loaded), Phase::Loading { proc, .. }) => {
                tracing::info!("model {} loaded (revision {}, {} ms)", loaded.model, loaded.revision, loaded.load_ms);
                self.revision = Some(loaded.revision);
                self.download = None;
                self.phase = Phase::Ready { proc };
            }
            (FromEngine::Error(error), phase @ (Phase::Starting { .. } | Phase::Loading { .. }))
                if error.id.is_none() =>
            {
                self.phase = phase;
                self.fail_model(now, format!("loading the model failed: {}", error.message), false);
            }
            (FromEngine::Result(result), Phase::Busy { proc, run }) if result.id == run.job.id => {
                self.phase = Phase::Ready { proc };
                let mut job = run.job;
                job.engine_time += now - run.sent;
                if !job.cancelled() {
                    self.crash_streak = 0;
                    let done = JobDone {
                        result,
                        queue_ms: ms(job.first_sent.unwrap_or(now) - job.enqueued),
                        load_wait_ms: ms(job.wait_total(now)),
                        engine_ms: ms(job.engine_time),
                        attempts: job.attempts,
                    };
                    job.finish(JobOutcome::Done(Box::new(done)));
                }
            }
            (FromEngine::Error(error), Phase::Busy { proc, run }) if error.id == Some(run.job.id) => {
                self.phase = Phase::Ready { proc };
                let mut job = run.job;
                job.engine_time += now - run.sent;
                if job.cancelled() {
                    return;
                }
                let attempts_left = job.attempts < MAX_ATTEMPTS;
                match error.code {
                    // The engine lost the stream; the next attempt sends the whole recording.
                    code if run.closes_stream && code != EngineErrorCode::Oom && attempts_left => {
                        tracing::warn!("the engine couldn't close the stream ({}); sending it whole", error.message);
                        job.status.set(JobPhase::Retrying);
                        self.queue.push_front(job);
                    }
                    EngineErrorCode::Oom if attempts_left => {
                        tracing::warn!("the engine ran out of GPU memory; trying once more");
                        job.oom_retry = true;
                        job.status.set(JobPhase::Retrying);
                        self.queue.push_front(job);
                    }
                    EngineErrorCode::Oom => {
                        job.fail(ErrorCode::Oom, "the GPU ran out of memory on both attempts", None);
                    }
                    _ if error.retryable && attempts_left => {
                        tracing::warn!("the engine failed ({}); trying once more", error.message);
                        job.status.set(JobPhase::Retrying);
                        self.queue.push_front(job);
                    }
                    _ => {
                        let code = if error.retryable { ErrorCode::EngineFailed } else { ErrorCode::Internal };
                        job.fail(code, format!("the engine failed: {}", error.message), None);
                    }
                }
            }
            // An `open` or `append` the engine refused: the stream is gone there.
            (FromEngine::Error(error), phase) if error.id.is_some_and(|id| self.streams.contains_key(&id)) => {
                self.phase = phase;
                let id = error.id.expect("checked above");
                tracing::warn!("the engine dropped stream {id}: {}", error.message);
                self.streams.remove(&id);
            }
            (msg, phase) => {
                self.phase = phase;
                tracing::debug!("ignoring `{}` from the engine", psst_engine_proto::Message::type_name(&msg));
            }
        }
    }

    /// The engine failed on its own: it exited, corrupted the stream, or overran an attempt.
    fn on_crash(&mut self, now: Instant, reason: String, exited: bool, timed_out: bool) {
        let (mut proc, run) = match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Starting { proc, .. } | Phase::Loading { proc, .. } | Phase::Ready { proc } => (proc, None),
            Phase::Busy { proc, run } => (proc, Some(run)),
            phase => {
                self.phase = phase;
                return;
            }
        };
        self.crash_streak += 1;
        tracing::warn!("the engine crashed ({reason}); {} in a row", self.crash_streak);
        let failed = self.crash_streak >= CRASH_LIMIT;
        if let Some(mut run) = run {
            run.job.engine_time += now - run.sent;
            if !run.job.cancelled() {
                if failed || run.job.attempts >= MAX_ATTEMPTS {
                    let (code, message) = if timed_out {
                        (ErrorCode::Timeout, "the engine ran out of time")
                    } else {
                        (ErrorCode::EngineFailed, "the engine crashed")
                    };
                    run.job.fail(code, message, None);
                } else {
                    run.job.status.set(JobPhase::Retrying);
                    self.queue.push_front(run.job);
                }
            }
        }
        let restart_after = backoff(self.crash_streak);
        self.phase = if exited {
            self.after_stop(now, restart_after)
        } else {
            proc.kill();
            Phase::Stopping { proc, stage: Stop::Kill, restart_after }
        };
        if failed {
            self.fail_model(now, format!("the engine crashed {CRASH_LIMIT} times in a row ({reason})"), true);
        }
    }

    fn on_timers(&mut self, now: Instant) {
        enum Due {
            LoadSilence,
            KillCancelled,
            AttemptOver(Duration),
        }
        let due = match &mut self.phase {
            Phase::Starting { silent_until, .. } | Phase::Loading { silent_until, .. } if now >= *silent_until => {
                Some(Due::LoadSilence)
            }
            Phase::Busy { run, .. } => match run.kill_at {
                Some(kill_at) if now >= kill_at => Some(Due::KillCancelled),
                None if now >= run.deadline => Some(Due::AttemptOver(run.deadline - run.sent)),
                _ => None,
            },
            Phase::Stopping { proc, stage, .. } => {
                match *stage {
                    Stop::Shutdown(at) if now >= at => {
                        proc.terminate();
                        *stage = Stop::Term(now + TERM_GRACE);
                    }
                    Stop::Term(at) if now >= at => {
                        proc.kill();
                        *stage = Stop::Kill;
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        };
        match due {
            Some(Due::LoadSilence) => {
                let error = format!("the engine made no loading progress for {} s", LOAD_SILENCE.as_secs());
                self.fail_model(now, error, true);
            }
            Some(Due::KillCancelled) => {
                tracing::info!("a cancelled request is still running; restarting the engine");
                self.kill_on_purpose();
            }
            Some(Due::AttemptOver(budget)) => {
                self.on_crash(now, format!("no answer within {} ms", budget.as_millis()), false, true);
            }
            None => {}
        }

        self.update_wait_clocks(now);
        let deadline = self.cfg.load_deadline;
        let mut i = 0;
        while i < self.queue.len() {
            let job = &self.queue[i];
            if job.waiting_since.is_some() && job.wait_total(now) >= deadline {
                let mut job = self.queue.remove(i).expect("index is in range");
                self.discard_stream(job.id);
                let message = format!("the model wasn't ready within {} s", deadline.as_secs());
                job.fail(ErrorCode::Timeout, message, None);
            } else {
                i += 1;
            }
        }
    }

    /// Starts the engine when it should run, and the next request when it's idle.
    fn drive(&mut self, now: Instant) {
        if self.target == Target::Loaded && !self.shutting_down {
            let start = match self.phase {
                Phase::Idle => true,
                Phase::Backoff(at) => now >= at,
                _ => false,
            };
            if start {
                self.spawn(now);
            }
        }
        if matches!(self.phase, Phase::Ready { .. })
            && let Some(mut job) = self.queue.pop_front()
        {
            let Phase::Ready { proc } = mem::replace(&mut self.phase, Phase::Idle) else { unreachable!() };
            job.attempts += 1;
            job.first_sent.get_or_insert(now);
            if let Some(since) = job.waiting_since.take() {
                job.waited += now - since;
            }
            job.status.set(JobPhase::Transcribing);
            let mut options = self.cfg.options;
            if job.oom_retry {
                options.batch_size = 1;
            }
            // A stream is closed once at most: a retry sends the whole recording.
            let closes_stream = job.streamed && self.streams.remove(&job.id) == Some(proc.generation);
            if closes_stream {
                proc.send(&ToEngine::Close(StreamRef { id: job.id }), Arc::default());
            } else {
                proc.send(
                    &ToEngine::Transcribe(Transcribe {
                        id: job.id,
                        language: job.language.clone(),
                        bias: job.bias.clone(),
                        options: options.to_json(),
                    }),
                    job.payload.clone(),
                );
            }
            let deadline = now + ATTEMPT_BASE + Duration::from_millis(job.audio_ms.into());
            let run = Run { job, sent: now, deadline, kill_at: None, closes_stream };
            self.phase = Phase::Busy { proc, run: Box::new(run) };
        }
    }

    fn spawn(&mut self, now: Instant) {
        self.generation += 1;
        self.download = None;
        // Streams live in one engine; requests for the old ones send their recordings whole.
        self.streams.clear();
        self.streams_supported = false;
        match EngineProcess::spawn(&self.cfg, self.generation, self.events.clone()) {
            Ok(proc) => self.phase = Phase::Starting { proc, silent_until: now + LOAD_SILENCE },
            Err(err) => {
                self.phase = Phase::Idle;
                let program = self.cfg.command.first().map(String::as_str).unwrap_or_default();
                self.fail_model(now, format!("cannot start the engine `{program}`: {err}"), true);
            }
        }
    }

    /// Ends the process on purpose. With `kill`, at once; otherwise `shutdown`, then `SIGTERM`,
    /// then `SIGKILL`. A running request is always killed at once.
    fn stop_engine(&mut self, now: Instant, kill: bool) {
        self.phase = match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle | Phase::Backoff(_) => Phase::Idle,
            Phase::Starting { mut proc, .. } | Phase::Loading { mut proc, .. } | Phase::Ready { mut proc } => {
                if kill {
                    proc.kill();
                    Phase::Stopping { proc, stage: Stop::Kill, restart_after: Duration::ZERO }
                } else {
                    proc.send(&ToEngine::Shutdown, Arc::default());
                    Phase::Stopping { proc, stage: Stop::Shutdown(now + SHUTDOWN_GRACE), restart_after: Duration::ZERO }
                }
            }
            Phase::Busy { mut proc, .. } => {
                proc.kill();
                Phase::Stopping { proc, stage: Stop::Kill, restart_after: Duration::ZERO }
            }
            stopping @ Phase::Stopping { .. } => stopping,
        };
    }

    /// Kills the engine to restart it; not a crash.
    fn kill_on_purpose(&mut self) {
        if let Phase::Busy { run, .. } = &mut self.phase
            && !run.job.cancelled()
        {
            // The attempt didn't fail; it runs again on the new engine.
            run.job.attempts -= 1;
        }
        self.phase = match mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Starting { mut proc, .. } | Phase::Loading { mut proc, .. } | Phase::Ready { mut proc } => {
                proc.kill();
                Phase::Stopping { proc, stage: Stop::Kill, restart_after: Duration::ZERO }
            }
            Phase::Busy { mut proc, run } => {
                proc.kill();
                let job = run.job;
                if !job.cancelled() {
                    job.status.set(JobPhase::Retrying);
                    self.queue.push_front(job);
                }
                Phase::Stopping { proc, stage: Stop::Kill, restart_after: Duration::ZERO }
            }
            phase => phase,
        };
    }

    fn restart_after_resume(&mut self) {
        if matches!(
            self.phase,
            Phase::Starting { .. } | Phase::Loading { .. } | Phase::Ready { .. } | Phase::Busy { .. }
        ) {
            tracing::info!("the machine woke up; restarting the engine");
            self.kill_on_purpose();
        }
    }

    /// Forgets a stream, telling the engine if it's still open there.
    fn discard_stream(&mut self, id: JobId) {
        if let Some(generation) = self.streams.remove(&id)
            && let Some(proc) = self.phase.proc()
            && proc.generation == generation
        {
            proc.send(&ToEngine::Discard(StreamRef { id }), Arc::default());
        }
    }

    fn after_stop(&self, now: Instant, restart_after: Duration) -> Phase {
        if self.target == Target::Loaded && !self.shutting_down {
            Phase::Backoff(now + restart_after)
        } else {
            Phase::Idle
        }
    }

    /// Marks the model failed: waiting requests fail, and the engine is stopped.
    fn fail_model(&mut self, now: Instant, error: String, kill: bool) {
        tracing::error!("model failed: {error}");
        let detail = json!({"reason": "failed", "error": error});
        self.fail_waiting(ErrorCode::ModelUnavailable, &format!("the model failed: {error}"), Some(detail));
        self.target = Target::Failed(error);
        self.download = None;
        self.stop_engine(now, kill);
    }

    /// Fails every queued request and the running one.
    fn fail_waiting(&mut self, code: ErrorCode, message: &str, detail: Option<Value>) {
        for mut job in self.queue.drain(..) {
            job.fail(code, message, detail.clone());
        }
        if let Phase::Busy { run, .. } = &mut self.phase {
            run.job.fail(code, message, detail);
        }
    }

    fn begin_shutdown(&mut self, now: Instant) {
        if !self.shutting_down {
            self.shutting_down = true;
            self.fail_waiting(ErrorCode::Internal, "the server is shutting down", None);
            self.stop_engine(now, false);
        }
    }

    /// Starts or stops each queued request's load-wait clock: it runs while the model isn't ready.
    fn update_wait_clocks(&mut self, now: Instant) {
        let ready = self.target == Target::Loaded && matches!(self.phase, Phase::Ready { .. } | Phase::Busy { .. });
        for job in &mut self.queue {
            match (ready, job.waiting_since) {
                (false, None) => job.waiting_since = Some(now),
                (true, Some(since)) => {
                    job.waited += now - since;
                    job.waiting_since = None;
                }
                _ => {}
            }
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        let phase = match &self.phase {
            Phase::Idle => None,
            Phase::Backoff(at) => Some(*at),
            Phase::Starting { silent_until, .. } | Phase::Loading { silent_until, .. } => Some(*silent_until),
            Phase::Ready { .. } => None,
            Phase::Busy { run, .. } => Some(run.kill_at.unwrap_or(run.deadline)),
            Phase::Stopping { stage: Stop::Shutdown(at) | Stop::Term(at), .. } => Some(*at),
            Phase::Stopping { stage: Stop::Kill, .. } => None,
        };
        let deadline = self.cfg.load_deadline;
        let jobs = self
            .queue
            .iter()
            .filter_map(|job| job.waiting_since.map(|since| since + deadline.saturating_sub(job.waited)));
        phase.into_iter().chain(jobs).min()
    }

    fn model_status(&self) -> ModelStatus {
        let (state, error) = match &self.target {
            Target::Failed(error) => (ModelState::Failed, Some(error.clone())),
            Target::Unloaded => (ModelState::Unloaded, None),
            Target::Loaded => match self.phase {
                Phase::Ready { .. } | Phase::Busy { .. } => (ModelState::Loaded, None),
                _ if self.download.is_some() => (ModelState::Downloading, None),
                _ => (ModelState::Loading, None),
            },
        };
        ModelStatus {
            name: self.cfg.model.clone(),
            state,
            revision: if state == ModelState::Loaded { self.revision.clone() } else { None },
            progress: if state == ModelState::Downloading { self.download } else { None },
            error,
        }
    }

    fn publish(&mut self) {
        let status = self.model_status();
        // An unload is answered once the engine is gone, or when a load overtook it.
        if self.phase.proc().is_none() || self.target != Target::Unloaded {
            for waiter in self.unload_waiters.drain(..) {
                let _ = waiter.send(status.clone());
            }
        }
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            if changed {
                *current = status;
            }
            changed
        });
    }
}

fn unloaded_message() -> &'static str {
    "the model was unloaded on purpose; run `psst-server model load`"
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
