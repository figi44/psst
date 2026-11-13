//! Messages on the client ↔ server WebSocket, `/v1/client`.
//!
//! Text frames carry one JSON message each, tagged by `type`. Receivers ignore fields they don't
//! know.
//!
//! Binary frames carry the active session's audio: 16 kHz mono, one 20 ms frame of 320 samples
//! per message, in the [`Codec`] its `start` named, the last frame padded with zeros. The client
//! sends them only after `ready` with `need_audio: true`, oldest first. The server keeps the audio
//! as 16-bit PCM whatever the codec, decoding Opus as each packet arrives with one decoder per
//! session, and counts samples after decoding (see [`StopMsg`]). A message that isn't a frame in
//! the session's codec ends the session with `error bad_audio`: for `pcm_s16le` an odd number of
//! bytes, for `opus` an empty message or one that isn't an Opus packet of exactly 20 ms.

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

/// `detail` of the `error protocol` that refuses a `hello`: what the server speaks, so the client
/// can tell which side to update. Servers from before it send no detail; they all speak
/// protocol 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolDetail {
    /// The protocol version the server speaks.
    pub protocol: u32,
    pub server_version: String,
}

/// The server's answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeMsg {
    pub server_version: String,
    /// The device this connection is authenticated as.
    pub device: DeviceId,
    pub limits: ServerLimits,
    /// The codecs `start.audio` may use. Absent from servers that predate Opus, which accept only
    /// `pcm_s16le`: they can't parse a `start` with another codec, and answer it with
    /// `error bad_request` without a `session_id`. So a client sends Opus only when it's listed.
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

/// `codec` is `pcm_s16le` or one more from `welcome.codecs`; anything but 16000 Hz in 20 ms frames
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

/// How a binary message holds its 20 ms frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// 320 samples as 16-bit little-endian PCM: 640 bytes.
    PcmS16le,
    /// One Opus packet that decodes to exactly 320 samples. Every frame has its packet: the
    /// encoder runs without DTX (and without FEC, which TCP doesn't need). Opus delays the audio
    /// by its 6.5 ms lookahead, so the decoded PCM starts with 6.5 ms of near-silence, and the last
    /// 6.5 ms of a recording, less its padding, are cut off.
    Opus,
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

/// Ends the audio of a session. `samples` counts the real (unpadded) samples, and the server
/// requires `samples ≤ received < samples + 320`, counting the samples it has decoded; otherwise
/// `error bad_audio`.
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
