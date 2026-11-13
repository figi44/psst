//! One client connection on `/v1/client` (plan 5.1): `hello`/`welcome`, then one active session
//! at a time. The connection buffers its session's audio; outcomes come back from the registry
//! as [`ConnEvent`]s, and `progress` goes out every 500 ms while a session is active.

use std::{sync::Arc, time::Duration};

use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use psst_config::{DEFAULT_PROFILE, Profile};
use psst_dict::Dictionary;
use psst_engine_proto::Bias;
use psst_pipeline::PipelineOptions;
use psst_proto::{
    DeviceId, ErrorCode, FRAME_SAMPLES, HistoryId, PROTOCOL_VERSION, SAMPLE_RATE, ServerTimings,
    api::ModelState,
    ws::{
        AudioFormat, CancelMsg, CancelledMsg, ClientMsg, ErrorMsg, FinalMsg, ProgressMsg, ProgressState,
        ReadyModelState, ReadyMsg, ReportMsg, ServerLimits, ServerMsg, StartMsg, StopMsg, WelcomeMsg,
    },
};
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, Interval, MissedTickBehavior},
};

use crate::{
    auth::{Identity, Refusal},
    engine::{JobOutcome, JobPhase, JobRequest, JobStatus},
    registry::{Attachment, Cancel, ConnEvent, Rejection, SessionKey, Start, StartRequest},
    state::AppState,
};

/// How long a new connection may take to send `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// `progress` interval.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// Audio more than this far past `max_recording_s` gets `too_long`.
const TOO_LONG_GRACE_S: u64 = 5;
const SAMPLES_PER_MS: u64 = SAMPLE_RATE as u64 / 1000;

/// Serves one upgraded WebSocket until it closes.
pub async fn run(state: Arc<AppState>, mut ws: WebSocket, identity: Result<Identity, Refusal>) {
    // Every refusal answers `hello`, so clients handle them all the same way.
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, ws.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<ClientMsg>(&text).ok(),
        Ok(Some(Ok(_))) => None,
        _ => return,
    };
    let identity = match identity {
        Ok(identity) => identity,
        Err(refusal) => {
            let message = match refusal {
                Refusal::Unauthorized => "missing, unknown or revoked device token",
                Refusal::Forbidden => "this user isn't in the server's local_users",
            };
            send(&mut ws, &ServerMsg::Error(ErrorMsg::new(None, ErrorCode::Unauthorized, message))).await;
            close(&mut ws, close_code::POLICY, "unauthorized").await;
            return;
        }
    };
    match hello {
        Some(ClientMsg::Hello(hello)) if hello.protocol == PROTOCOL_VERSION => {}
        Some(ClientMsg::Hello(hello)) => {
            let message =
                format!("the server speaks protocol {PROTOCOL_VERSION}; the client speaks {}", hello.protocol);
            send(&mut ws, &ServerMsg::Error(ErrorMsg::new(None, ErrorCode::Protocol, message))).await;
            close(&mut ws, close_code::PROTOCOL, "protocol").await;
            return;
        }
        _ => {
            let message = "the first message must be `hello`";
            send(&mut ws, &ServerMsg::Error(ErrorMsg::new(None, ErrorCode::BadRequest, message))).await;
            close(&mut ws, close_code::PROTOCOL, "expected hello").await;
            return;
        }
    }

    let device = identity.device();
    let (tx, mut events) = mpsc::unbounded_channel();
    let conn_id = state.conns.next_id();
    let attachment = Attachment { conn_id, tx };
    let recorded = match &identity {
        Identity::Local { user } => state.db.touch_local(user),
        Identity::Token { device_id } => {
            state.conns.replace(&device, attachment.clone());
            state.db.touch(device_id)
        }
    };
    if let Err(err) = recorded {
        tracing::error!("cannot record device {device}: {err:#}");
    }
    let welcome = WelcomeMsg {
        server_version: env!("CARGO_PKG_VERSION").into(),
        device: device.clone(),
        limits: ServerLimits { max_recording_s: state.server.limits.max_recording_s },
    };
    tracing::debug!("connection {conn_id} from {device}");
    let mut conn = Conn { state: state.clone(), device, attachment, active: None };
    if !send(&mut ws, &ServerMsg::Welcome(welcome)).await {
        return;
    }

    let mut shutdown = state.shutdown.clone();
    loop {
        let flow = tokio::select! {
            msg = ws.recv() => match msg {
                Some(Ok(Message::Text(text))) => conn.on_text(&mut ws, &text).await,
                Some(Ok(Message::Binary(bytes))) => conn.on_audio(&mut ws, &bytes).await,
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => Flow::Closed,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => Flow::Continue,
            },
            Some(event) = events.recv() => conn.on_event(&mut ws, event).await,
            _ = tick(&mut conn.active) => conn.on_tick(&mut ws).await,
            _ = stopped(&mut shutdown) => {
                close(&mut ws, close_code::AWAY, "the server is shutting down").await;
                Flow::Closed
            }
        };
        if let Flow::Closed = flow {
            break;
        }
    }
    conn.disconnected();
}

enum Flow {
    Continue,
    Closed,
}

struct Conn {
    state: Arc<AppState>,
    device: DeviceId,
    attachment: Attachment,
    active: Option<Active>,
}

/// The connection's one active session.
struct Active {
    key: SessionKey,
    epoch: u64,
    phase: Phase,
    progress: Interval,
}

enum Phase {
    Recording(Box<Recording>),
    /// Queued or running; the outcome arrives as an event.
    Waiting {
        status: Arc<JobStatus>,
        audio_ms: u32,
    },
}

struct Recording {
    pcm: Vec<u8>,
    last_frame: Instant,
    history_id: HistoryId,
    profile: Profile,
    dictionary: Arc<Dictionary>,
    /// False for `eval.bias = false`: no prompt and no dictionary stage.
    bias: bool,
}

async fn stopped(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

async fn tick(active: &mut Option<Active>) {
    match active {
        Some(active) => {
            active.progress.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn progress_interval() -> Interval {
    let mut interval = tokio::time::interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

fn error(session_id: Option<psst_proto::SessionId>, code: ErrorCode, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error(ErrorMsg::new(session_id, code, message))
}

impl Conn {
    async fn on_text(&mut self, ws: &mut WebSocket, text: &str) -> Flow {
        let msg = match serde_json::from_str::<ClientMsg>(text) {
            Ok(msg) => msg,
            Err(err) => {
                return self
                    .reply(ws, error(None, ErrorCode::BadRequest, format!("cannot parse message: {err}")))
                    .await;
            }
        };
        match msg {
            ClientMsg::Hello(_) => {
                self.reply(ws, error(None, ErrorCode::BadRequest, "`hello` was already received")).await
            }
            ClientMsg::Start(start) => self.on_start(ws, start).await,
            ClientMsg::Stop(stop) => self.on_stop(ws, stop).await,
            ClientMsg::Cancel(cancel) => self.on_cancel(ws, cancel).await,
            ClientMsg::Report(report) => {
                self.on_report(report);
                Flow::Continue
            }
        }
    }

    async fn reply(&mut self, ws: &mut WebSocket, msg: ServerMsg) -> Flow {
        if send(ws, &msg).await { Flow::Continue } else { Flow::Closed }
    }

    async fn on_start(&mut self, ws: &mut WebSocket, start: StartMsg) -> Flow {
        let session_id = start.session_id;
        let reject = |code, message: &str| error(Some(session_id), code, message);
        if self.active.is_some() {
            return self
                .reply(ws, reject(ErrorCode::BadRequest, "a session is already active on this connection"))
                .await;
        }
        if start.audio != AudioFormat::default() {
            let message = "audio must be pcm_s16le at 16000 Hz in 20 ms frames";
            return self.reply(ws, reject(ErrorCode::BadRequest, message)).await;
        }
        if start.eval.is_some() && !self.state.server.eval_mode {
            let message = "`eval` is accepted only when the server runs with eval_mode = true";
            return self.reply(ws, reject(ErrorCode::BadRequest, message)).await;
        }
        let profile_name = start.profile.as_deref().unwrap_or(DEFAULT_PROFILE);
        let Some(profile) = self.state.live.profile(profile_name) else {
            return self.reply(ws, reject(ErrorCode::BadRequest, &format!("unknown profile `{profile_name}`"))).await;
        };
        let dictionary = self.state.live.dictionary();
        let key = SessionKey { device: self.device.clone(), session_id };
        let state = self.state.clone();
        let admit = move || admit(&state);
        let started = self.state.registry.start(
            &self.state.db,
            StartRequest {
                key: key.clone(),
                resume: start.resume,
                profile: &profile.name,
                conn: self.attachment.clone(),
                admit: &admit,
            },
        );
        let model_state = if self.state.model_loaded() { ReadyModelState::Loaded } else { ReadyModelState::Loading };
        let ready = |resumed, need_audio| ServerMsg::Ready(ReadyMsg { session_id, model_state, resumed, need_audio });
        match started {
            Err(err) => {
                tracing::error!("cannot start session {session_id}: {err:#}");
                self.reply(ws, reject(ErrorCode::Internal, "the server couldn't record the session")).await
            }
            Ok(Start::Rejected(rejection)) => {
                tracing::info!("session {session_id} of {}: refused ({:?})", self.device, rejection.code);
                self.reply(ws, ServerMsg::Error(rejection.into_msg(session_id))).await
            }
            Ok(Start::Record { epoch, history_id }) => {
                tracing::info!("session {session_id} of {}: recording", self.device);
                let recording = Recording {
                    pcm: Vec::with_capacity(10 * SAMPLE_RATE as usize * 2),
                    last_frame: Instant::now(),
                    history_id,
                    profile,
                    dictionary,
                    bias: start.eval.is_none_or(|eval| eval.bias),
                };
                self.active = Some(Active {
                    key,
                    epoch,
                    phase: Phase::Recording(Box::new(recording)),
                    progress: progress_interval(),
                });
                self.reply(ws, ready(false, true)).await
            }
            Ok(Start::Finished(msg)) => {
                tracing::info!("session {session_id} of {}: resumed, already finished", self.device);
                if let Flow::Closed = self.reply(ws, ready(true, false)).await {
                    return Flow::Closed;
                }
                self.reply(ws, ServerMsg::Final(Box::new((*msg).clone()))).await
            }
            Ok(Start::Attached { epoch, status, audio_ms }) => {
                tracing::info!("session {session_id} of {}: resumed, attached to its request", self.device);
                self.active = Some(Active {
                    key,
                    epoch,
                    phase: Phase::Waiting { status, audio_ms },
                    progress: progress_interval(),
                });
                self.reply(ws, ready(true, false)).await
            }
        }
    }

    async fn on_audio(&mut self, ws: &mut WebSocket, bytes: &[u8]) -> Flow {
        let Some(active) = &mut self.active else { return Flow::Continue };
        let Phase::Recording(recording) = &mut active.phase else { return Flow::Continue };
        let (key, epoch) = (active.key.clone(), active.epoch);
        if !bytes.len().is_multiple_of(2) {
            return self
                .end_recording(ws, &key, epoch, ErrorCode::BadAudio, "an audio frame has an odd number of bytes")
                .await;
        }
        recording.pcm.extend_from_slice(bytes);
        recording.last_frame = Instant::now();
        let limit_s = u64::from(self.state.server.limits.max_recording_s) + TOO_LONG_GRACE_S;
        if recording.pcm.len() as u64 / 2 > limit_s * u64::from(SAMPLE_RATE) {
            let message = format!(
                "the recording is more than {TOO_LONG_GRACE_S} s past max_recording_s = {} s",
                self.state.server.limits.max_recording_s
            );
            return self.end_recording(ws, &key, epoch, ErrorCode::TooLong, &message).await;
        }
        Flow::Continue
    }

    /// Ends the active recording with an error.
    async fn end_recording(
        &mut self,
        ws: &mut WebSocket,
        key: &SessionKey,
        epoch: u64,
        code: ErrorCode,
        message: &str,
    ) -> Flow {
        tracing::info!("session {} of {}: {message}", key.session_id, key.device);
        self.state.registry.drop_recording(key, epoch);
        self.active = None;
        self.reply(ws, error(Some(key.session_id), code, message)).await
    }

    async fn on_stop(&mut self, ws: &mut WebSocket, stop: StopMsg) -> Flow {
        let Some(active) = self.active.as_mut().filter(|active| active.key.session_id == stop.session_id) else {
            tracing::debug!("ignoring `stop` for session {}, which isn't active here", stop.session_id);
            return Flow::Continue;
        };
        let Phase::Recording(recording) = &active.phase else { return Flow::Continue };
        let (key, epoch) = (active.key.clone(), active.epoch);
        let received = recording.pcm.len() as u64 / 2;
        if !(stop.samples <= received && received < stop.samples + FRAME_SAMPLES as u64) {
            let message = format!("stop.samples = {}, but {received} samples arrived", stop.samples);
            return self.end_recording(ws, &key, epoch, ErrorCode::BadAudio, &message).await;
        }

        let stop_at = Instant::now();
        let real = &recording.pcm[..stop.samples as usize * 2];
        let payload: Vec<u8> = real
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|pair| (f32::from(i16::from_le_bytes(*pair)) / 32768.0).to_le_bytes())
            .collect();
        let profile = &recording.profile;
        let bias = if recording.bias {
            Bias { style: profile.prompt_style.clone(), terms: recording.dictionary.bias_terms(&profile.language) }
        } else {
            Bias::default()
        };
        let audio_ms = u32::try_from(stop.samples / SAMPLES_PER_MS).unwrap_or(u32::MAX);
        let request = JobRequest { language: profile.language.clone(), bias, payload, audio_ms };
        let engine = &self.state.engine;
        let Some(handle) =
            self.state.registry.stop(&key, epoch, self.attachment.conn_id, audio_ms, || engine.submit(request))
        else {
            self.active = None;
            return Flow::Continue;
        };
        tracing::info!("session {} of {}: stopped after {audio_ms} ms", key.session_id, key.device);

        let Phase::Recording(recording) =
            std::mem::replace(&mut active.phase, Phase::Waiting { status: handle.status.clone(), audio_ms })
        else {
            unreachable!()
        };
        let job = SessionJob {
            key,
            epoch,
            history_id: recording.history_id,
            profile: recording.profile,
            dictionary: recording.dictionary,
            pipeline: PipelineOptions { dictionary: recording.bias },
            audio_ms,
            stop_at,
        };
        tokio::spawn(job.finish(self.state.clone(), handle.outcome));
        Flow::Continue
    }

    async fn on_cancel(&mut self, ws: &mut WebSocket, cancel: CancelMsg) -> Flow {
        let Some(active) = self.active.as_ref().filter(|active| active.key.session_id == cancel.session_id) else {
            tracing::debug!("ignoring a late `cancel` for session {}", cancel.session_id);
            return Flow::Continue;
        };
        match self.state.registry.cancel(&active.key, active.epoch, self.attachment.conn_id) {
            Cancel::Cancelled(job) => {
                tracing::info!("session {} of {}: cancelled", cancel.session_id, self.device);
                if let Some(job) = job {
                    self.state.engine.cancel(job);
                }
                self.active = None;
                self.reply(ws, ServerMsg::Cancelled(CancelledMsg { session_id: cancel.session_id })).await
            }
            // The outcome is already on its way.
            Cancel::Ignored => Flow::Continue,
        }
    }

    fn on_report(&self, report: ReportMsg) {
        match self.state.db.report(&self.device, report.session_id, report.history_id, &report.timings) {
            Ok(true) => {}
            Ok(false) => tracing::debug!("ignoring a report for history row {} of another session", report.history_id),
            Err(err) => tracing::error!("cannot store a report: {err:#}"),
        }
    }

    async fn on_event(&mut self, ws: &mut WebSocket, event: ConnEvent) -> Flow {
        match event {
            ConnEvent::Msg(msg) => {
                let ends = match &msg {
                    ServerMsg::Final(fin) => Some(fin.session_id),
                    ServerMsg::Error(err) => err.session_id,
                    ServerMsg::Cancelled(cancelled) => Some(cancelled.session_id),
                    _ => None,
                };
                if ends.is_some() && self.active.as_ref().map(|active| active.key.session_id) == ends {
                    self.active = None;
                }
                self.reply(ws, msg).await
            }
            ConnEvent::Detached { session_id } => {
                if self.active.as_ref().is_some_and(|active| active.key.session_id == session_id) {
                    // Its outcome now goes to the other connection, so this one can't end the
                    // session with a terminal message; closing it is how its client learns.
                    tracing::info!("session {session_id} of {} was resumed on another connection", self.device);
                    self.active = None;
                    close(ws, close_code::NORMAL, "the session was resumed on another connection").await;
                    return Flow::Closed;
                }
                Flow::Continue
            }
            ConnEvent::Close { reason } => {
                tracing::info!("closing a connection of {}: {reason}", self.device);
                close(ws, close_code::NORMAL, &reason).await;
                Flow::Closed
            }
        }
    }

    async fn on_tick(&mut self, ws: &mut WebSocket) -> Flow {
        let Some(active) = &self.active else { return Flow::Continue };
        let loaded = self.state.engine.status().state == ModelState::Loaded;
        let (state, received_ms) = match &active.phase {
            Phase::Recording(recording) => {
                let state = if loaded { ProgressState::Recording } else { ProgressState::Loading };
                (state, recording.pcm.len() as u64 / 2 / SAMPLES_PER_MS)
            }
            Phase::Waiting { status, audio_ms } => {
                let state = match status.get() {
                    JobPhase::Queued if loaded => ProgressState::Queued,
                    JobPhase::Queued => ProgressState::Loading,
                    JobPhase::Transcribing => ProgressState::Transcribing,
                    JobPhase::Retrying => ProgressState::Retrying,
                };
                (state, u64::from(*audio_ms))
            }
        };
        let session_id = active.key.session_id;
        self.reply(ws, ServerMsg::Progress(ProgressMsg { session_id, state, received_ms })).await
    }

    fn disconnected(&mut self) {
        if let Some(active) = self.active.take() {
            let last_frame = match &active.phase {
                Phase::Recording(recording) => recording.last_frame,
                Phase::Waiting { .. } => Instant::now(),
            };
            tracing::info!(
                "session {} of {}: connection lost; the session continues",
                active.key.session_id,
                self.device
            );
            self.state.registry.detach(&active.key, active.epoch, self.attachment.conn_id, last_frame);
        }
        self.state.conns.remove(&self.device, self.attachment.conn_id);
    }
}

/// Whether a new recording may begin, from the model's state (plan 6.2).
fn admit(state: &AppState) -> Result<(), Rejection> {
    let status = state.engine.status();
    let unavailable =
        |message: String, detail| Rejection { code: ErrorCode::ModelUnavailable, message, detail: Some(detail) };
    match status.state {
        ModelState::Loaded | ModelState::Loading => Ok(()),
        ModelState::Unloaded => Err(Rejection::new(
            ErrorCode::ModelUnloaded,
            "the model was unloaded on purpose; run `psst-server model load`",
        )),
        ModelState::Downloading => {
            let progress = status.progress.unwrap_or(psst_proto::api::DownloadProgress { bytes: 0, total: None });
            Err(unavailable(
                "the model is downloading".into(),
                json!({"reason": "downloading", "bytes": progress.bytes, "total": progress.total}),
            ))
        }
        ModelState::Missing => Err(unavailable("the model is missing".into(), json!({"reason": "missing"}))),
        ModelState::Failed => {
            let error = status.error.unwrap_or_default();
            Err(unavailable(format!("the model failed: {error}"), json!({"reason": "failed", "error": error})))
        }
    }
}

/// A stopped session waiting for its transcription.
struct SessionJob {
    key: SessionKey,
    epoch: u64,
    history_id: HistoryId,
    profile: Profile,
    dictionary: Arc<Dictionary>,
    pipeline: PipelineOptions,
    audio_ms: u32,
    stop_at: Instant,
}

impl SessionJob {
    async fn finish(self, state: Arc<AppState>, outcome: oneshot::Receiver<JobOutcome>) {
        let session_id = self.key.session_id;
        let outcome = outcome.await.unwrap_or(JobOutcome::Failed {
            code: ErrorCode::Internal,
            message: "the engine supervisor stopped".into(),
            detail: None,
        });
        match outcome {
            JobOutcome::Cancelled => {}
            JobOutcome::Failed { code, message, detail } => {
                tracing::warn!("session {session_id} of {}: {message}", self.key.device);
                let error = ErrorMsg { detail, ..ErrorMsg::new(Some(session_id), code, message) };
                state.registry.fail(&self.key, self.epoch, error);
            }
            JobOutcome::Done(done) => {
                let started = Instant::now();
                let fin = psst_pipeline::run(&done.result.trace, &self.profile, &self.dictionary, &self.pipeline);
                let pipeline_ms = millis(started.elapsed());
                let timings = ServerTimings {
                    audio_ms: self.audio_ms,
                    queue_ms: done.queue_ms,
                    load_wait_ms: done.load_wait_ms,
                    engine_ms: done.engine_ms,
                    attempts: done.attempts,
                    pipeline_ms,
                    stop_to_final_ms: millis(self.stop_at.elapsed()),
                };
                let trace = done.result.trace;
                let msg = FinalMsg {
                    session_id,
                    history_id: self.history_id,
                    r#final: fin.clone(),
                    timings,
                    trace: state.server.eval_mode.then(|| trace.clone()),
                };
                if state.registry.finish(&self.key, self.epoch, msg) {
                    tracing::info!(
                        "session {session_id} of {}: final after {} ms ({} attempts)",
                        self.key.device,
                        timings.stop_to_final_ms,
                        timings.attempts
                    );
                    if let Err(err) =
                        state.db.finish_session(self.history_id, &trace.model, &trace.revision, &fin, &timings)
                    {
                        tracing::error!("cannot store history row {}: {err:#}", self.history_id);
                    }
                }
            }
        }
    }
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().try_into().unwrap_or(u32::MAX)
}

/// Sends a message; false if the connection is gone.
async fn send(ws: &mut WebSocket, msg: &ServerMsg) -> bool {
    let text = serde_json::to_string(msg).expect("server messages serialise");
    ws.send(Message::Text(text.into())).await.is_ok()
}

/// Closes with a close handshake, so the client reads everything sent before.
async fn close(ws: &mut WebSocket, code: u16, reason: &str) {
    let frame = CloseFrame { code, reason: reason.to_owned().into() };
    if ws.send(Message::Close(Some(frame))).await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(Ok(msg)) = ws.recv().await {
            if let Message::Close(_) = msg {
                break;
            }
        }
    })
    .await;
}
