use anyhow::Context;
use psst_pipeline::{PipelineOptions, run};

use crate::{
    baseline::{Baseline, CaseOutput, CaseResult},
    cases::EvalSet,
    metrics::{Scoring, score},
    stress,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Replay {
    pub baseline: Baseline,
    pub unrecorded: Vec<String>,
}

pub fn replay(set: &EvalSet) -> anyhow::Result<Replay> {
    let mut results = Vec::new();
    let mut unrecorded = Vec::new();
    for case in &set.cases {
        let Some((output, raw)) = case.traces()? else {
            unrecorded.push(case.id.clone());
            continue;
        };
        let profile = set.profile_for(&case.lang).with_context(|| format!("case {}", case.id))?;
        let scoring = Scoring {
            reference: &case.reference,
            terms: &case.terms,
            fillers: &profile.fillers,
            every_dictionary_edit_counts: case.terms.is_empty(),
        };
        let replay = |trace, dictionary| -> anyhow::Result<CaseOutput> {
            let out = run(trace, &profile, &set.dictionary, &PipelineOptions { dictionary });
            let counts = score(&scoring, &out).with_context(|| format!("case {}", case.id))?;
            Ok(CaseOutput { text: out.text, raw_text: out.raw_text, edits: out.edits, counts })
        };
        results.push(CaseResult {
            id: case.id.clone(),
            source: case.source,
            lang: case.lang.clone(),
            split: case.split,
            must_pass: case.must_pass,
            output: replay(output, true)?,
            raw: replay(raw, false)?,
        });
    }
    Ok(Replay { baseline: Baseline::new(results, stress::stress(set)?), unrecorded })
}
