//! `evals/baseline.json`: every case's output with its counts, the metrics per group, and the
//! stress test's results. The gate recomputes metrics from the per-case counts, so it can compare
//! any subset of cases.

use std::{collections::BTreeMap, fmt, fs, io, path::Path};

use anyhow::Context;
use psst_proto::Edit;
use serde::{Deserialize, Serialize};

use crate::{
    cases::{Source, Split, write_atomic},
    metrics::Counts,
};

/// Version of the file's layout.
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub format: u32,
    /// Metrics per source, language and split, then per source.
    pub summary: Vec<GroupSummary>,
    /// Every case with recorded traces, by id.
    pub cases: Vec<CaseResult>,
    /// The stress test, per language.
    pub stress: Vec<StressResult>,
}

impl Baseline {
    pub fn new(mut cases: Vec<CaseResult>, mut stress: Vec<StressResult>) -> Self {
        cases.sort_by(|a, b| a.id.cmp(&b.id));
        stress.sort_by(|a, b| a.lang.cmp(&b.lang));
        Self { format: FORMAT, summary: summarise(&cases), cases, stress }
    }

    /// Reads a baseline; `None` if the file doesn't exist.
    pub fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        let baseline: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a baseline in format {FORMAT}", path.display()))?;
        Ok(Some(baseline))
    }

    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let mut text = serde_json::to_string_pretty(self).expect("baselines serialise");
        text.push('\n');
        write_atomic(path, text.as_bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.cases.is_empty() && self.stress.is_empty()
    }

    pub fn case(&self, id: &str) -> Option<&CaseResult> {
        self.cases.binary_search_by(|c| c.id.as_str().cmp(id)).ok().map(|i| &self.cases[i])
    }
}

/// A case's two replays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub id: String,
    pub source: Source,
    pub lang: String,
    pub split: Split,
    pub must_pass: bool,
    /// `engine_output` through the pipeline with the dictionary: what the author would get.
    pub output: CaseOutput,
    /// `engine_output_raw` through the pipeline without the dictionary.
    pub raw: CaseOutput,
}

impl CaseResult {
    pub fn group(&self) -> Group {
        Group { source: self.source, lang: self.lang.clone(), split: self.split }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOutput {
    pub text: String,
    pub raw_text: String,
    pub edits: Vec<Edit>,
    pub counts: Counts,
}

/// A source, language and split: the unit metrics are reported and gated in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Group {
    pub source: Source,
    pub lang: String,
    pub split: Split,
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.source, self.lang, self.split)
    }
}

/// The metrics of a set of outputs, with the counts they come from. Percentages and rates are
/// rounded to three decimals.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Percent; `null` without term occurrences.
    pub term_recall: Option<f64>,
    /// Percent; `null` without reference words.
    pub wer: Option<f64>,
    pub false_corrections_per_1000: f64,
    #[serde(flatten)]
    pub counts: Counts,
}

impl From<Counts> for Metrics {
    fn from(counts: Counts) -> Self {
        Self {
            term_recall: counts.term_recall().map(round),
            wer: counts.wer().map(round),
            false_corrections_per_1000: round(counts.false_corrections_per_1000()),
            counts,
        }
    }
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupSummary {
    pub source: Source,
    /// `*` for the total over a source.
    pub lang: String,
    /// `*` for the total over a source.
    pub split: String,
    pub cases: usize,
    pub output: Metrics,
    pub raw: Metrics,
}

/// Sums the counts of `cases` per group.
pub fn group_counts<'a>(cases: impl IntoIterator<Item = &'a CaseResult>) -> BTreeMap<Group, (usize, Counts, Counts)> {
    let mut groups: BTreeMap<Group, (usize, Counts, Counts)> = BTreeMap::new();
    for case in cases {
        let entry = groups.entry(case.group()).or_default();
        entry.0 += 1;
        entry.1 += case.output.counts;
        entry.2 += case.raw.counts;
    }
    groups
}

fn summarise(cases: &[CaseResult]) -> Vec<GroupSummary> {
    let mut rows = Vec::new();
    let mut totals: BTreeMap<Source, (usize, Counts, Counts)> = BTreeMap::new();
    for (group, (count, output, raw)) in group_counts(cases) {
        let total = totals.entry(group.source).or_default();
        total.0 += count;
        total.1 += output;
        total.2 += raw;
        rows.push(GroupSummary {
            source: group.source,
            lang: group.lang,
            split: group.split.to_string(),
            cases: count,
            output: output.into(),
            raw: raw.into(),
        });
    }
    for (source, (count, output, raw)) in totals {
        rows.push(GroupSummary {
            source,
            lang: "*".into(),
            split: "*".into(),
            cases: count,
            output: output.into(),
            raw: raw.into(),
        });
    }
    rows
}

/// The stress test's results for one language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressResult {
    pub lang: String,
    pub sentences: usize,
    pub metrics: Metrics,
    /// Sentences with a false correction, for review.
    pub flagged: Vec<FlaggedSentence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlaggedSentence {
    /// 1-based line in `stress/<lang>.txt`.
    pub line: usize,
    pub sentence: String,
    pub output: String,
    pub edits: Vec<Edit>,
}
