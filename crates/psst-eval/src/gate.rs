//! The regression gate: the pull request's replay against main's `baseline.json`, on
//! the case ids present in both, and the absolute floors.
//!
//! Going past a tolerance on purpose needs the `baseline-change` label on the pull request,
//! which only a maintainer applies. The label never waives stale traces, an inconsistent baseline, or
//! the floors.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::Context;
use serde_json::Value;

use crate::{
    baseline::{Baseline, CaseResult, Group, StressResult, group_counts},
    cases::Split,
    metrics::Counts,
};

/// The label that lets a pull request go past the tolerances. Agents never apply it.
pub const BASELINE_CHANGE_LABEL: &str = "baseline-change";

/// Largest allowed drop of term recall, in percentage points.
pub const TERM_RECALL_DROP: f64 = 0.5;
/// Largest allowed rise of false corrections per 1,000 output words.
pub const FALSE_CORRECTIONS_RISE: f64 = 0.2;
/// Largest allowed rise of WER, in percentage points.
pub const WER_RISE: f64 = 0.2;

/// Whether the absolute floors fail the gate. They're computed and printed either way;
/// `--floors` enforces them while this is off.
pub const FLOORS_ENFORCED: bool = false;
/// Most false corrections per 1,000 words a floor allows.
pub const FLOOR_FALSE_CORRECTIONS: f64 = 2.0;
/// The share of raw's term errors the dictionary has to remove.
pub const FLOOR_TERM_ERROR_REDUCTION: f64 = 0.5;
/// Most WER a floor allows above raw, in percentage points.
pub const FLOOR_WER_ABOVE_RAW: f64 = 0.2;

const EPSILON: f64 = 1e-9;

/// The pull request's cases against main's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comparison {
    /// Cases present in both.
    pub common: usize,
    /// Cases only in the pull request; they get the consistency check and the floors.
    pub new: Vec<String>,
    /// Cases only on main.
    pub removed: Vec<String>,
    pub regressions: Vec<String>,
}

/// Compares the cases present in both baselines: any change of a must-pass case's output, and
/// the tolerances per source, language and split.
pub fn compare_cases(main: &Baseline, pr: &Baseline) -> Comparison {
    let mut comparison = Comparison::default();
    let mut pairs: Vec<(&CaseResult, &CaseResult)> = Vec::new();
    for case in &pr.cases {
        match main.case(&case.id) {
            Some(before) => pairs.push((before, case)),
            None => comparison.new.push(case.id.clone()),
        }
    }
    comparison.removed = main.cases.iter().filter(|c| pr.case(&c.id).is_none()).map(|c| c.id.clone()).collect();
    comparison.common = pairs.len();

    for (before, after) in &pairs {
        if !(before.must_pass || after.must_pass) {
            continue;
        }
        for (run, old, new) in
            [("output", &before.output.text, &after.output.text), ("raw output", &before.raw.text, &after.raw.text)]
        {
            if old != new {
                comparison
                    .regressions
                    .push(format!("must-pass case {}: {run} changed from {old:?} to {new:?}", after.id));
            }
        }
    }

    // Grouped by the pull request's metadata, so both sides sum the same cases.
    let mut groups: BTreeMap<Group, (Counts, Counts)> = BTreeMap::new();
    for (before, after) in &pairs {
        let entry = groups.entry(after.group()).or_default();
        entry.0 += before.output.counts;
        entry.1 += after.output.counts;
    }
    for (group, (before, after)) in groups {
        comparison.regressions.extend(tolerance_failures(&group.to_string(), &before, &after));
    }
    comparison
}

fn tolerance_failures(what: &str, before: &Counts, after: &Counts) -> Vec<String> {
    let mut failures = Vec::new();
    if let (Some(old), Some(new)) = (before.term_recall(), after.term_recall())
        && new < old - TERM_RECALL_DROP - EPSILON
    {
        failures.push(format!("{what}: term recall fell from {old:.2}% to {new:.2}% (tolerance {TERM_RECALL_DROP})"));
    }
    let (old, new) = (before.false_corrections_per_1000(), after.false_corrections_per_1000());
    if new > old + FALSE_CORRECTIONS_RISE + EPSILON {
        failures.push(format!(
            "{what}: false corrections rose from {old:.2} to {new:.2} per 1,000 words (tolerance {FALSE_CORRECTIONS_RISE})"
        ));
    }
    if let (Some(old), Some(new)) = (before.wer(), after.wer())
        && new > old + WER_RISE + EPSILON
    {
        failures.push(format!("{what}: WER rose from {old:.2}% to {new:.2}% (tolerance {WER_RISE})"));
    }
    failures
}

/// Compares the stress test per language present in both: false corrections may rise by at
/// most [`FALSE_CORRECTIONS_RISE`] per 1,000 words.
pub fn compare_stress(main: &[StressResult], pr: &[StressResult]) -> Vec<String> {
    let mut failures = Vec::new();
    for after in pr {
        let Some(before) = main.iter().find(|s| s.lang == after.lang) else { continue };
        let (old, new) =
            (before.metrics.counts.false_corrections_per_1000(), after.metrics.counts.false_corrections_per_1000());
        if new > old + FALSE_CORRECTIONS_RISE + EPSILON {
            failures.push(format!(
                "stress/{}: false corrections rose from {old:.2} to {new:.2} per 1,000 words (tolerance {FALSE_CORRECTIONS_RISE})",
                after.lang
            ));
        }
    }
    failures
}

/// Acceptance as absolute floors, on the held-out half per source and language,
/// compared with raw.
pub fn case_floors(pr: &Baseline) -> Vec<String> {
    let holdout = pr.cases.iter().filter(|c| c.split == Split::Holdout);
    let mut failures = Vec::new();
    for (group, (_, output, raw)) in group_counts(holdout) {
        if let (Some(raw_recall), Some(recall)) = (raw.term_recall(), output.term_recall()) {
            let (raw_errors, errors) = (100.0 - raw_recall, 100.0 - recall);
            if raw_errors <= EPSILON && errors > EPSILON {
                failures
                    .push(format!("{group}: term recall is {recall:.2}%; raw has no term errors, so it must be 100%"));
            } else if raw_errors > EPSILON && errors > (1.0 - FLOOR_TERM_ERROR_REDUCTION) * raw_errors + EPSILON {
                failures.push(format!(
                    "{group}: term errors are {errors:.2}% against raw's {raw_errors:.2}%; they must be at most half"
                ));
            }
        }
        let rate = output.false_corrections_per_1000();
        if rate > FLOOR_FALSE_CORRECTIONS + EPSILON {
            failures.push(format!(
                "{group}: {rate:.2} false corrections per 1,000 words; the floor is {FLOOR_FALSE_CORRECTIONS}"
            ));
        }
        if let (Some(raw_wer), Some(wer)) = (raw.wer(), output.wer())
            && wer > raw_wer + FLOOR_WER_ABOVE_RAW + EPSILON
        {
            failures.push(format!(
                "{group}: WER is {wer:.2}%, more than {FLOOR_WER_ABOVE_RAW} points above raw's {raw_wer:.2}%"
            ));
        }
    }
    failures
}

/// The stress floor: at most [`FLOOR_FALSE_CORRECTIONS`] per 1,000 words in every language.
pub fn stress_floors(stress: &[StressResult]) -> Vec<String> {
    stress
        .iter()
        .filter_map(|s| {
            let rate = s.metrics.counts.false_corrections_per_1000();
            (rate > FLOOR_FALSE_CORRECTIONS + EPSILON).then(|| {
                format!(
                    "stress/{}: {rate:.2} false corrections per 1,000 words; the floor is {FLOOR_FALSE_CORRECTIONS}",
                    s.lang
                )
            })
        })
        .collect()
}

/// Whether the pull request in a GitHub Actions event payload (`$GITHUB_EVENT_PATH`) carries
/// [`BASELINE_CHANGE_LABEL`]. Events without a pull request carry no labels.
pub fn has_baseline_change_label(event: &Path) -> anyhow::Result<bool> {
    let text = fs::read_to_string(event).with_context(|| format!("cannot read {}", event.display()))?;
    let event: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not a GitHub event payload", event.display()))?;
    let labels = event.pointer("/pull_request/labels").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    Ok(labels.iter().any(|label| label.get("name").and_then(Value::as_str) == Some(BASELINE_CHANGE_LABEL)))
}
