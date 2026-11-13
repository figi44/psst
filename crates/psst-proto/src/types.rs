use std::ops::Range;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineResult {
    pub language: String,
    /// Kept by the engine's VAD, padding included; `0` when it found no speech.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_ms: Option<u32>,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Untrimmed: Whisper starts it with a space.
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_speech_prob: Option<f32>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineTrace {
    pub engine: String,
    pub model: String,
    pub revision: String,
    pub path: String,
    pub options: Value,
    pub prompt: String,
    pub result: EngineResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Final {
    pub text: String,
    pub raw_text: String,
    /// In pipeline order.
    pub edits: Vec<Edit>,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub stage: Stage,
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<TermId>,
    /// UTF-8 byte offsets into [`Final::raw_text`]. A dictionary edit covers exactly the characters
    /// it replaced, a rules edit the raw span of every token it touches.
    pub raw_span: Range<usize>,
    pub before: String,
    pub after: String,
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

/// A term's `text` after NFC and case folding.
pub type TermId = String;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerTimings {
    pub audio_ms: u32,
    /// From `stop` until the engine took the request.
    pub queue_ms: u32,
    pub load_wait_ms: u32,
    /// Across all attempts.
    pub engine_ms: u32,
    pub attempts: u32,
    pub pipeline_ms: u32,
    pub stop_to_final_ms: u32,
}

/// A field is absent when the dictation had no such step (a `retry` has no toggle).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// `detail` is a [`crate::ws::ProtocolDetail`].
    Protocol,
    BadRequest,
    Unauthorized,
    Busy,
    /// Audio more than 5 s past `max_recording_s`.
    TooLong,
    BadAudio,
    /// Unloaded on purpose.
    ModelUnloaded,
    /// Missing, downloading or failed.
    ModelUnavailable,
    Timeout,
    EngineFailed,
    Oom,
    Internal,
}

impl ErrorCode {
    pub fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::BadAudio | Self::Timeout | Self::EngineFailed | Self::Oom | Self::Internal)
    }
}
