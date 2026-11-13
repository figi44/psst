//! Builders for pipeline tests.
#![allow(dead_code)]

use std::ops::Range;

use psst_config::{ConfigFile, DictionaryFile, Profile, ProfilesFile, Rule};
use psst_dict::Dictionary;
use psst_pipeline::{PipelineOptions, run};
use psst_proto::{Edit, EngineResult, EngineTrace, Final, Segment, Stage};

pub fn segment(text: &str) -> Segment {
    Segment { text: text.into(), start_ms: 0, end_ms: 0, no_speech_prob: None, words: None }
}

/// A segment with the engine's `no_speech_prob`.
pub fn segment_p(text: &str, no_speech_prob: f32) -> Segment {
    Segment { no_speech_prob: Some(no_speech_prob), ..segment(text) }
}

pub fn trace(language: &str, speech_ms: Option<u32>, segments: Vec<Segment>) -> EngineTrace {
    EngineTrace {
        engine: "fake".into(),
        model: "m".into(),
        revision: "r".into(),
        path: "sequential".into(),
        options: serde_json::json!({}),
        prompt: String::new(),
        result: EngineResult { language: language.into(), speech_ms, segments },
    }
}

/// One English segment with plenty of speech, the way Whisper writes it (leading space).
pub fn said(text: &str) -> EngineTrace {
    trace("en", Some(3000), vec![segment(&format!(" {text}"))])
}

/// One Czech segment with plenty of speech.
pub fn said_cs(text: &str) -> EngineTrace {
    trace("cs", Some(3000), vec![segment(&format!(" {text}"))])
}

/// The default profile, in `language`, with the built-in fillers for it.
pub fn profile(language: &str) -> Profile {
    let mut file = ProfilesFile::default();
    file.profiles.get_mut("default").unwrap().language = language.into();
    file.resolve("default").unwrap()
}

pub fn profile_with(language: &str, rules: &[Rule], fillers: &[&str]) -> Profile {
    Profile { rules: rules.to_vec(), fillers: fillers.iter().map(|f| f.to_string()).collect(), ..profile(language) }
}

pub fn dictionary(toml: &str) -> Dictionary {
    Dictionary::from_file(&DictionaryFile::from_toml_str(toml).unwrap()).unwrap()
}

/// Runs the default profile for the trace's language.
pub fn run_with(trace: &EngineTrace, dict: &str) -> Final {
    run(trace, &profile(&trace.result.language), &dictionary(dict), &PipelineOptions::default())
}

/// The final text with the default profile.
pub fn text(trace: &EngineTrace, dict: &str) -> String {
    run_with(trace, dict).text
}

pub fn edit(stage: Stage, rule: &str, raw_span: Range<usize>, before: &str, after: &str) -> Edit {
    Edit { stage, rule: rule.into(), term: None, raw_span, before: before.into(), after: after.into(), score: None }
}

pub fn exact(raw_span: Range<usize>, term: &str, before: &str, after: &str) -> Edit {
    Edit { term: Some(term.into()), ..edit(Stage::Dictionary, "exact", raw_span, before, after) }
}

pub fn rule(rule: &str, raw_span: Range<usize>, before: &str, after: &str) -> Edit {
    edit(Stage::Rules, rule, raw_span, before, after)
}

pub fn guard(rule: &str, raw_span: Range<usize>, before: &str) -> Edit {
    edit(Stage::Guard, rule, raw_span, before, "")
}
