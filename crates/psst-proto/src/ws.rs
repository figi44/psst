//! Messages on the client ↔ server WebSocket, `/v1/client` (plan 5.1).
//!
//! Text frames carry one JSON message each, tagged by `type`. Binary frames carry audio:
//! 20 ms of `pcm_s16le` (640 bytes) per frame. Receivers ignore fields they don't know.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ClientTimings, DeviceId, EngineTrace, ErrorCode, FRAME_MS, Final, HistoryId, SAMPLE_RATE, ServerTimings, SessionId,
};

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(HelloMsg),
    Start(StartMsg),
    Stop(StopMsg),
    Cancel(CancelMsg),
    Report(ReportMsg),
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome(WelcomeMsg),
    Ready(ReadyMsg),
    Progress(ProgressMsg),
    Final(Box<FinalMsg>),
    Error(ErrorMsg),
    Cancelled(CancelledMsg),
}

/// First message on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMsg {
    pub protocol: u32,
    pub client: ClientInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// The server's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeMsg {
    pub server_version: String,
    /// The device this connection is authenticated as.
    pub device: DeviceId,
    pub limits: ServerLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    pub max_recording_s: u32,
}

/// Starts a session, or resumes one with `resume: true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartMsg {
    pub session_id: SessionId,
    /// Profile name; the server uses `default` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub audio: AudioFormat,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
    /// Accepted only when the server runs with `eval_mode = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<EvalOpts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    pub frame_ms: u32,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self { codec: Codec::PcmS16le, sample_rate: SAMPLE_RATE, frame_ms: FRAME_MS }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    PcmS16le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Streams while recording.
    Live,
    /// Sends a finished recording as fast as the link allows.
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalOpts {
    /// `false` sends no prompt to the engine (not even `prompt_style`) and skips the dictionary.
    pub bias: bool,
}

/// Ends the audio of a session. The server requires `samples ≤ received < samples + 320`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopMsg {
    pub session_id: SessionId,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelMsg {
    pub session_id: SessionId,
}

/// The client's timings for a finished session. Gets no reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportMsg {
    pub session_id: SessionId,
    pub history_id: HistoryId,
    pub timings: ClientTimings,
}

/// Sent as soon as `start` arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyMsg {
    pub session_id: SessionId,
    pub model_state: ReadyModelState,
    pub resumed: bool,
    /// Whether the client must send the session's audio.
    pub need_audio: bool,
}

/// The model's state as seen by a session that was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyModelState {
    Loaded,
    Loading,
}

/// Sent every 500 ms from `ready` until the session's terminal message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressMsg {
    pub session_id: SessionId,
    pub state: ProgressState,
    /// Audio received so far.
    pub received_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Recording,
    Queued,
    Loading,
    Transcribing,
    Retrying,
}

/// A session's result. Terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalMsg {
    pub session_id: SessionId,
    pub history_id: HistoryId,
    pub r#final: Final,
    pub timings: ServerTimings,
    /// Only with `eval_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<EngineTrace>,
}

/// An error. Terminal for its session when `session_id` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMsg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub code: ErrorCode,
    pub message: String,
    /// Machine-readable context, for example download progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    pub retryable: bool,
}

impl ErrorMsg {
    /// An error whose `retryable` follows [`ErrorCode::retryable`].
    pub fn new(session_id: Option<SessionId>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { session_id, code, message: message.into(), detail: None, retryable: code.retryable() }
    }
}

/// The session was cancelled. Terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelledMsg {
    pub session_id: SessionId,
}
