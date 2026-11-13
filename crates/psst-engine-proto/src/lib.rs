//! A frame is an 8-byte prefix (header length, then payload length, both `u32` little-endian),
//! a UTF-8 JSON header with a `type`, and an optional binary payload. For audio the payload is
//! `f32` little-endian mono samples at 16 kHz.
//!
//! # Streams
//!
//! A stream is an `open`, its `append`s, then a `close` (answered as `transcribe` would answer the
//! whole recording) or a `discard` (not answered). Only engines whose `hello` has
//! [`Capabilities::stream`] get them.
//!
//! - Engines take appends at any time, also while answering another request.
//! - An `append` or `close` for a stream that isn't open gets `error bad_request`, and so does a
//!   stream longer than `max_audio_s` (which is then discarded). A `discard` of a stream that isn't
//!   open does nothing.

pub mod fake;
pub mod frame;

use psst_proto::EngineTrace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

pub use frame::{FrameError, FrameLimits, RawFrame, read_frame, read_frame_async, write_frame, write_frame_async};

pub const PROTOCOL_VERSION: u32 = 1;

pub const PROTOCOL_ENV: &str = "PSST_ENGINE_PROTOCOL";

pub trait Message: Serialize + DeserializeOwned {
    const TYPES: &'static [&'static str];

    fn type_name(&self) -> &'static str;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToEngine {
    Load(Load),
    /// Payload: the whole recording.
    Transcribe(Transcribe),
    Open(Transcribe),
    /// Payload: the stream's next samples.
    Append(StreamRef),
    Close(StreamRef),
    Discard(StreamRef),
    Shutdown,
}

impl Message for ToEngine {
    const TYPES: &'static [&'static str] = &["load", "transcribe", "open", "append", "close", "discard", "shutdown"];

    fn type_name(&self) -> &'static str {
        match self {
            Self::Load(_) => "load",
            Self::Transcribe(_) => "transcribe",
            Self::Open(_) => "open",
            Self::Append(_) => "append",
            Self::Close(_) => "close",
            Self::Discard(_) => "discard",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromEngine {
    Hello(Hello),
    Progress(Progress),
    Loaded(Loaded),
    Result(TranscribeResult),
    Error(EngineError),
}

impl Message for FromEngine {
    const TYPES: &'static [&'static str] = &["hello", "progress", "loaded", "result", "error"];

    fn type_name(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Progress(_) => "progress",
            Self::Loaded(_) => "loaded",
            Self::Result(_) => "result",
            Self::Error(_) => "error",
        }
    }
}

/// The engine's first message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub engine: String,
    pub version: String,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub bias: bool,
    /// Also sets the frame payload limit.
    pub max_audio_s: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Load {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub device: String,
    pub compute_type: String,
}

/// Sent while loading; the server fails the load after 60 s without `progress` or `loaded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: LoadPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadPhase {
    Download,
    Load,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loaded {
    pub model: String,
    /// The resolved revision (a Hugging Face commit for faster-whisper).
    pub revision: String,
    pub load_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcribe {
    /// Echoed in the `result` or `error`.
    pub id: u64,
    pub language: String,
    pub bias: Bias,
    /// From the model config; the engine resolves `path`.
    pub options: Value,
}

/// The `id` of the stream's `open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRef {
    pub id: u64,
}

/// Empty for eval runs without bias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bias {
    /// Used as written.
    pub style: String,
    /// Most important first; the engine drops them from the end to fit.
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscribeResult {
    pub id: u64,
    pub trace: EngineTrace,
    pub timings: EngineTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTimings {
    pub decode_ms: u32,
    pub prompt_tokens: u32,
    pub bias_terms_used: u32,
    /// Whether `style` alone was over the budget and was cut from its start.
    pub prompt_truncated: bool,
    /// Only when answering `close`: the chunks of up to 30 s the recording was decoded in.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunks: u32,
    /// Of `chunks`, those decoded before `close`, while it was still recording.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunks_reused: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineError {
    /// The request's id, when the error answers a `transcribe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub code: EngineErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineErrorCode {
    /// The request's `type` is unknown to the engine.
    Unsupported,
    /// The header isn't valid JSON, or a known `type` has missing or invalid fields.
    BadRequest,
    /// `transcribe` before a successful `load`.
    NotLoaded,
    LoadFailed,
    /// The engine keeps running.
    Oom,
    Internal,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported message type `{0}`")]
    UnknownType(String),
    #[error("malformed message: {0}")]
    Malformed(#[source] serde_json::Error),
}

pub fn decode<M: Message>(header: &[u8]) -> Result<M, DecodeError> {
    #[derive(Deserialize)]
    struct Tag {
        r#type: String,
    }
    serde_json::from_slice(header).map_err(|err| match serde_json::from_slice::<Tag>(header) {
        Ok(tag) if !M::TYPES.contains(&tag.r#type.as_str()) => DecodeError::UnknownType(tag.r#type),
        _ => DecodeError::Malformed(err),
    })
}

pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    serde_json::to_vec(msg).expect("protocol messages always serialise")
}

pub fn samples_to_payload(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

pub fn payload_to_samples(payload: &[u8]) -> Option<Vec<f32>> {
    let (samples, rest) = payload.as_chunks::<4>();
    rest.is_empty().then(|| samples.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect())
}
