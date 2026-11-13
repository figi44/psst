//! Text frames carry one JSON message each, tagged by `type`. Receivers ignore fields they don't
//! know.
//!
//! Binary frames carry the active session's audio: one 20 ms frame per message, in the [`Codec`]
//! its `start` named, the last frame padded with zeros. The client sends them only after `ready`
//! with `need_audio: true`, oldest first. The server decodes Opus with one decoder per session. A
//! message that isn't a frame in the session's codec ends the session with `error bad_audio`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ClientTimings, DeviceId, EngineTrace, ErrorCode, FRAME_MS, Final, HistoryId, SAMPLE_RATE, ServerTimings, SessionId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(HelloMsg),
    Start(StartMsg),
    Stop(StopMsg),
    Cancel(CancelMsg),
    Report(ReportMsg),
}

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

/// First message on every connection. A server that doesn't speak `protocol` answers with
/// `error protocol` and a [`ProtocolDetail`], then closes the connection.
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

/// Servers that predate it send no detail; they all speak protocol 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolDetail {
    pub protocol: u32,
    pub server_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeMsg {
    pub server_version: String,
    pub device: DeviceId,
    pub limits: ServerLimits,
    /// Absent from servers that predate Opus: they answer a `start` with any codec but `pcm_s16le`
    /// with `error bad_request` without a `session_id`, so send Opus only when it's listed.
    #[serde(default = "pcm_only")]
    pub codecs: Vec<Codec>,
}

fn pcm_only() -> Vec<Codec> {
    vec![Codec::PcmS16le]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    pub max_recording_s: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartMsg {
    pub session_id: SessionId,
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

/// `codec` is `pcm_s16le` or one from `welcome.codecs`; anything but 16000 Hz in 20 ms frames
/// gets `error bad_request`.
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
    /// One packet per frame, decoding to exactly 320 samples, so the encoder runs without DTX.
    /// Opus's 6.5 ms lookahead delays the decoded audio and cuts off a recording's last 6.5 ms,
    /// less its padding.
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Live,
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalOpts {
    /// `false` sends no prompt to the engine (not even `prompt_style`) and skips the dictionary.
    pub bias: bool,
}

/// `samples` counts the real (unpadded) samples, and the server requires
/// `samples ≤ received < samples + 320`, counted after decoding; otherwise `error bad_audio`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopMsg {
    pub session_id: SessionId,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelMsg {
    pub session_id: SessionId,
}

/// Gets no reply.
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
    pub need_audio: bool,
}

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

/// Terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalMsg {
    pub session_id: SessionId,
    pub history_id: HistoryId,
    pub r#final: Final,
    pub timings: ServerTimings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<EngineTrace>,
}

/// Terminal for its session when `session_id` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMsg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    pub retryable: bool,
}

impl ErrorMsg {
    pub fn new(session_id: Option<SessionId>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { session_id, code, message: message.into(), detail: None, retryable: code.retryable() }
    }
}

/// Terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelledMsg {
    pub session_id: SessionId,
}
