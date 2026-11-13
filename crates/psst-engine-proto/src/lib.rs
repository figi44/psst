//! The server ↔ engine protocol: typed messages and the frame codec used on the
//! engine's stdin and stdout.
//!
//! A frame is an 8-byte prefix (header length, then payload length, both `u32` little-endian),
//! a UTF-8 JSON header with a `type`, and an optional binary payload. For audio the payload is
//! `f32` little-endian mono samples at 16 kHz.
//!
//! - [`frame`]: reading and writing raw frames, with the size limits.
//! - [`ToEngine`] / [`FromEngine`]: the messages in each direction; [`decode`] and [`encode`]
//!   convert between them and frame headers.
//! - [`fake`]: the scripted engine behind `psst-server __fake-engine`.

pub mod fake;
pub mod frame;

use psst_proto::EngineTrace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

pub use frame::{FrameError, FrameLimits, RawFrame, read_frame, read_frame_async, write_frame, write_frame_async};

/// Version of the engine protocol: sent by the engine in `hello`, and given to it in
/// [`PROTOCOL_ENV`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Environment variable through which the server tells the engine the protocol version.
pub const PROTOCOL_ENV: &str = "PSST_ENGINE_PROTOCOL";

/// A message that can travel in a frame header.
pub trait Message: Serialize + DeserializeOwned {
    /// Every `type` this direction knows.
    const TYPES: &'static [&'static str];

    /// This message's `type`.
    fn type_name(&self) -> &'static str;
}

/// Server → engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToEngine {
    Load(Load),
    /// The payload is the whole recording.
    Transcribe(Transcribe),
    Shutdown,
}

impl Message for ToEngine {
    const TYPES: &'static [&'static str] = &["load", "transcribe", "shutdown"];

    fn type_name(&self) -> &'static str {
        match self {
            Self::Load(_) => "load",
            Self::Transcribe(_) => "transcribe",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Engine → server.
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
    /// Engine name, for example `faster-whisper`.
    pub engine: String,
    pub version: String,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Whether the engine applies `transcribe.bias`.
    pub bias: bool,
    /// Longest recording the engine accepts; also sets its frame payload limit.
    pub max_audio_s: u32,
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
    /// Chosen by the server; echoed in the `result` or `error`.
    pub id: u64,
    pub language: String,
    pub bias: Bias,
    /// Engine options from the model config; the engine resolves `path`.
    pub options: Value,
}

/// What the engine should expect to hear. Empty for eval runs without bias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bias {
    /// The profile's `prompt_style`, used as written.
    pub style: String,
    /// Dictionary terms, most important first; the engine drops them from the end to fit.
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
    /// Loading or downloading the model failed.
    LoadFailed,
    /// GPU out of memory; the engine keeps running.
    Oom,
    Internal,
}

/// Why a frame header couldn't be turned into a message.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// A well-formed header with a `type` this direction doesn't know.
    #[error("unsupported message type `{0}`")]
    UnknownType(String),
    /// Not JSON, no `type`, or a known `type` with missing or invalid fields.
    #[error("malformed message: {0}")]
    Malformed(#[source] serde_json::Error),
}

/// Parses a frame header into a message.
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

/// Serialises a message as a frame header.
pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    serde_json::to_vec(msg).expect("protocol messages always serialise")
}

/// Encodes samples as a frame payload: `f32` little-endian.
pub fn samples_to_payload(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Decodes a frame payload into samples; `None` if its length isn't a multiple of 4.
pub fn payload_to_samples(payload: &[u8]) -> Option<Vec<f32>> {
    let (samples, rest) = payload.as_chunks::<4>();
    rest.is_empty().then(|| samples.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect())
}
