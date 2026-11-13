//! The stress test (plan 7.3, layer 3): ordinary sentences that contain no dictionary terms, as
//! engine results without probabilities, through the pipeline with the dictionary. Every
//! dictionary edit is a false correction.
//!
//! `evals/stress/<lang>.txt` holds one sentence per line; blank lines and lines starting with `#`
//! are skipped. Without the directory there's nothing to test.

use std::{fs, io};

use anyhow::Context;
use psst_pipeline::{PipelineOptions, run};
use psst_proto::{EngineResult, EngineTrace, Segment};

use crate::{
    baseline::{FlaggedSentence, StressResult},
    cases::EvalSet,
    metrics::{Counts, Scoring, score},
};

/// Runs every `stress/<lang>.txt`, in language order.
pub fn stress(set: &EvalSet) -> anyhow::Result<Vec<StressResult>> {
    let dir = set.dir.join("stress");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.with_context(|| format!("cannot read {}", dir.display()))?.path();
        if path.extension().is_some_and(|ext| ext == "txt") {
            files.push(path);
        }
    }
    files.sort();

    let mut results = Vec::new();
    for path in files {
        let lang = path.file_stem().and_then(|s| s.to_str()).context("stress file names are <lang>.txt")?.to_owned();
        let profile = set.profile_for(&lang).with_context(|| format!("{}", path.display()))?;
        let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut counts = Counts::default();
        let mut sentences = 0;
        let mut flagged = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let sentence = line.trim();
            if sentence.is_empty() || sentence.starts_with('#') {
                continue;
            }
            sentences += 1;
            let out = run(&trace(&lang, sentence), &profile, &set.dictionary, &PipelineOptions { dictionary: true });
            let scoring = Scoring {
                reference: sentence,
                terms: &[],
                fillers: &profile.fillers,
                every_dictionary_edit_counts: true,
            };
            let sentence_counts = score(&scoring, &out).with_context(|| format!("{}:{}", path.display(), index + 1))?;
            counts += sentence_counts;
            if sentence_counts.false_corrections > 0 {
                flagged.push(FlaggedSentence {
                    line: index + 1,
                    sentence: sentence.to_owned(),
                    output: out.text,
                    edits: out.edits,
                });
            }
        }
        results.push(StressResult { lang, sentences, metrics: counts.into(), flagged });
    }
    Ok(results)
}

/// A sentence as the engine would return it, without probabilities or word timings.
fn trace(lang: &str, sentence: &str) -> EngineTrace {
    EngineTrace {
        engine: "stress".into(),
        model: "stress".into(),
        revision: "stress".into(),
        path: "batched".into(),
        options: serde_json::json!({}),
        prompt: String::new(),
        result: EngineResult {
            language: lang.into(),
            speech_ms: None,
            segments: vec![Segment {
                text: format!(" {sentence}"),
                start_ms: 0,
                end_ms: 0,
                no_speech_prob: None,
                words: None,
            }],
        },
    }
}
