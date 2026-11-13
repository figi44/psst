//! Human-readable tables of a baseline's metrics, on stdout.

use crate::baseline::{Baseline, Metrics, StressResult};

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |v| format!("{v:.1}"))
}

fn pair(output: String, raw: String) -> String {
    format!("{output:>6} ({raw:>5})")
}

/// Prints the metrics per group, with raw in parentheses.
pub fn print_summary(baseline: &Baseline) {
    if baseline.cases.is_empty() {
        println!("no cases with recorded traces");
    } else {
        println!(
            "{:<24} {:>5}  {:>14}  {:>14}  {:>14}",
            "source/lang/split", "cases", "recall % (raw)", "WER % (raw)", "FC/1k (raw)"
        );
        for row in &baseline.summary {
            let (output, raw): (&Metrics, &Metrics) = (&row.output, &row.raw);
            println!(
                "{:<24} {:>5}  {:>14}  {:>14}  {:>14}",
                format!("{}/{}/{}", row.source, row.lang, row.split),
                row.cases,
                pair(percent(output.term_recall), percent(raw.term_recall)),
                pair(percent(output.wer), percent(raw.wer)),
                pair(
                    format!("{:.2}", output.false_corrections_per_1000),
                    format!("{:.2}", raw.false_corrections_per_1000)
                ),
            );
        }
    }
    print_stress(&baseline.stress);
}

/// Prints the stress test per language.
pub fn print_stress(stress: &[StressResult]) {
    if stress.is_empty() {
        println!("no stress sentences");
        return;
    }
    println!("{:<8} {:>9}  {:>8}  {:>6}", "stress", "sentences", "words", "FC/1k");
    for result in stress {
        println!(
            "{:<8} {:>9}  {:>8}  {:>6.2}",
            result.lang,
            result.sentences,
            result.metrics.counts.output_words,
            result.metrics.false_corrections_per_1000
        );
        for flagged in &result.flagged {
            println!("  line {}: {:?} → {:?}", flagged.line, flagged.sentence, flagged.output);
        }
    }
}
