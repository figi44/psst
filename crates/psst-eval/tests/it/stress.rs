//! `psst-eval stress`: ordinary sentences through the dictionary, gated like replay.

use crate::common;

use common::Evals;
use psst_eval::{
    baseline::{FlaggedSentence, StressResult},
    gate::compare_stress,
    metrics::Counts,
};

#[test]
fn without_stress_sentences_it_passes_trivially() {
    let evals = Evals::new();
    let main = evals.file("main-baseline.json");
    std::fs::write(&main, r#"{"format": 1, "summary": [], "cases": [], "stress": []}"#).unwrap();
    let run = evals.run("stress", &["--check", "--against", main.to_str().unwrap()]);
    run.success();
    assert!(run.stdout().contains("no stress sentences"), "{}", run.stdout());
}

#[test]
fn sentences_are_counted_per_language_and_kept_in_the_baseline() {
    let evals = Evals::new();
    evals.write("stress/en.txt", "# split B\nThe weather was fine all week.\n\nShe sold the old car.\n");
    evals.write("stress/cs.txt", "Venku prší.\n");
    evals.write_baseline();
    let run = evals.run("stress", &["--check"]);
    run.success();
    let stdout = run.stdout();
    assert!(stdout.lines().any(|l| l.split_whitespace().take(2).eq(["cs", "1"])), "{stdout}");
    assert!(stdout.lines().any(|l| l.split_whitespace().take(3).eq(["en", "2", "11"])), "{stdout}");
}

#[test]
fn stress_results_that_differ_from_the_baseline_fail_the_check() {
    let evals = Evals::new();
    evals.write("stress/en.txt", "The weather was fine all week.\n");
    evals.write_baseline();
    evals.write("stress/en.txt", "The weather was fine all week.\nShe sold the old car.\n");
    evals.run("stress", &["--check"]).fails_with("the stress results in");
    evals.run("replay", &["--check"]).fails_with("doesn't match this replay (the stress results differ)");
}

fn result(lang: &str, false_corrections: u64, output_words: u64) -> StressResult {
    let counts = Counts { false_corrections, output_words, ref_words: output_words, ..Counts::default() };
    StressResult { lang: lang.into(), sentences: 1, metrics: counts.into(), flagged: Vec::<FlaggedSentence>::new() }
}

#[test]
fn false_corrections_may_rise_by_at_most_0_2_per_1000_words() {
    let main = [result("en", 2, 10_000), result("cs", 0, 10_000)];
    assert!(compare_stress(&main, &[result("en", 4, 10_000), result("cs", 2, 10_000)]).is_empty());
    assert_eq!(
        compare_stress(&main, &[result("en", 5, 10_000), result("cs", 0, 10_000), result("de", 50, 100)]),
        ["stress/en: false corrections rose from 0.20 to 0.50 per 1,000 words (tolerance 0.2)"]
    );
}
