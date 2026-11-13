//! The text pipeline (plan 6.4): guard → dictionary → rules. Pure; the server and every eval
//! call the same [`run`].
//!
//! This is the contract stub: [`run`] builds `raw_text` (step 1) and returns it unchanged. The
//! guard, dictionary and rules arrive with the pipeline work package.

use psst_config::Profile;
use psst_dict::Dictionary;
use psst_proto::{EngineTrace, Final};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineOptions {
    /// Apply the dictionary. `false` for eval raw runs.
    pub dictionary: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self { dictionary: true }
    }
}

/// Turns an engine's trace into the final text for a profile.
pub fn run(trace: &EngineTrace, profile: &Profile, dict: &Dictionary, opts: &PipelineOptions) -> Final {
    let _ = (profile, dict, opts);
    let raw_text = raw_text(trace);
    Final { text: raw_text.clone(), raw_text, edits: Vec::new(), language: trace.result.language.clone() }
}

/// Step 1: the segment texts, each trimmed, joined with single spaces.
fn raw_text(trace: &EngineTrace) -> String {
    let texts: Vec<&str> = trace.result.segments.iter().map(|s| s.text.trim()).filter(|t| !t.is_empty()).collect();
    texts.join(" ")
}
