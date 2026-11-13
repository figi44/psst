//! Shared types: what an engine returns, what the pipeline makes of it, timings and
//! error codes.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The engine's transcription of one recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineResult {
    /// Language the engine transcribed in (ISO 639-1, for example `en`).
    pub language: String,
    /// Audio kept by the engine's VAD, padding included; `0` when VAD found no speech.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_ms: Option<u32>,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Text exactly as the engine produced it (Whisper starts it with a space).
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_speech_prob: Option<f32>,
    /// Present only when the engine ran with word timestamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<Word>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub prob: f32,
}

/// Everything needed to replay a transcription through the pipeline: what ran, how, and what it
/// returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineTrace {
    pub engine: String,
    pub model: String,
    pub revision: String,
    /// Decoding path actually taken (`batched` for faster-whisper).
    pub path: String,
    /// Effective engine options for this request.
    pub options: Value,
    /// Prompt (and hotwords) actually used.
    pub prompt: String,
    pub result: EngineResult,
}

/// The pipeline's output for one dictation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Final {
    /// The text to insert.
    pub text: String,
    /// The engine's text before any pipeline step: segment texts, trimmed, joined with spaces.
    pub raw_text: String,
    /// Every change from `raw_text` to `text`, in pipeline order.
    pub edits: Vec<Edit>,
    pub language: String,
}

/// One change made by a pipeline stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub stage: Stage,
    /// Id of the rule or check that made the edit (for example `fillers`, `exact`, `fuzzy`).
    pub rule: String,
    /// The dictionary term involved, for dictionary edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<TermId>,
    /// UTF-8 byte offsets into [`Final::raw_text`]. A dictionary edit covers exactly the characters
    /// it replaced, a rules edit the raw span of every token it touches.
    pub raw_span: Range<usize>,
    pub before: String,
    pub after: String,
    /// Match score, for fuzzy dictionary edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Guard,
    Dictionary,
    Rules,
}

/// A dictionary term's identity key: its `text` after NFC and case folding.
pub type TermId = String;

/// Server-side stage durations for one dictation, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerTimings {
    /// Length of the recording.
    pub audio_ms: u32,
    /// From `stop` until the engine took the request.
    pub queue_ms: u32,
    /// Time spent waiting for the model to load.
    pub load_wait_ms: u32,
    /// Engine time across all attempts.
    pub engine_ms: u32,
    /// Engine attempts made (1 or 2).
    pub attempts: u32,
    pub pipeline_ms: u32,
    /// From `stop` arriving until `final` was sent.
    pub stop_to_final_ms: u32,
}

/// Client-side durations for one dictation, in milliseconds on the client's monotonic clock.
/// A field is absent when the dictation had no such step (for example a `retry` has no toggle).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientTimings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toggle_to_overlay_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toggle_to_first_frame_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_to_final_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_to_dispatch_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_to_dispatch_ms: Option<u32>,
}

/// Why the server ended a session or refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Incompatible protocol version.
    Protocol,
    BadRequest,
    Unauthorized,
    /// Too many active sessions.
    Busy,
    /// Audio more than 5 s past `max_recording_s`.
    TooLong,
    /// `stop.samples` doesn't match the audio received.
    BadAudio,
    /// The model was unloaded on purpose.
    ModelUnloaded,
    /// The model is missing, downloading or failed.
    ModelUnavailable,
    Timeout,
    EngineFailed,
    Oom,
    Internal,
}

impl ErrorCode {
    /// Whether the same request may succeed if the client sends it again.
    pub fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::BadAudio | Self::Timeout | Self::EngineFailed | Self::Oom | Self::Internal)
    }
}
