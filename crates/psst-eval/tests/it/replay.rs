use crate::common;

use common::{CS_PROMPT, Evals, case, trace};
use psst_eval::cases::{Case, Split};

const REFERENCE: &str = "Push the fix to psst before lunch.";

fn main_with(cases: &[Case]) -> (Evals, String) {
    let evals = Evals::new();
    evals.set_cases(cases);
    evals.write_baseline();
    let main = evals.save_main_baseline();
    (evals, main)
}

fn ci(evals: &Evals, main: &str, labels: &[&str]) -> common::Run {
    evals.write_baseline();
    let event = evals.event(labels);
    evals.run("replay", &["--check", "--against", main, "--event", &event])
}

#[test]
fn an_unchanged_replay_passes_the_gate() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    let run = ci(&evals, &main, &[]);
    run.success();
    assert!(run.stdout().contains("gate: 1 cases in both baselines, 0 new, 0 only on main"), "{}", run.stdout());
}

#[test]
fn a_worse_transcript_fails_the_gate_on_wer() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], "Push the fax to psst before lunch.", REFERENCE)]);
    ci(&evals, &main, &[]).fails_with("synthetic/en/tune: WER rose from 0.00% to 14.29%");
}

#[test]
fn a_lost_term_fails_the_gate_on_term_recall() {
    let reference = "Hyprland and t3code both run on NixOS.";
    let (evals, main) = main_with(&[case("en-1", reference, &["Hyprland", "t3code"], reference, reference)]);
    evals.set_cases(&[case(
        "en-1",
        reference,
        &["Hyprland", "t3code"],
        "Hyprland and the code both run on NixOS.",
        reference,
    )]);
    ci(&evals, &main, &[]).fails_with("synthetic/en/tune: term recall fell from 100.00% to 50.00%");
}

#[test]
fn any_change_to_a_must_pass_case_fails_the_gate_even_an_improvement() {
    let must_pass = |heard: &str| Case { must_pass: true, ..case("en-1", REFERENCE, &["psst"], heard, heard) };
    let (evals, main) = main_with(&[must_pass("Push the fax to psst before lunch.")]);
    evals.set_cases(&[must_pass(REFERENCE)]);
    ci(&evals, &main, &[]).fails_with("must-pass case en-1: output changed");
}

#[test]
fn new_cases_are_not_gated_against_main() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.set_cases(&[
        case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE),
        Case { must_pass: true, ..case("en-2", "Hyprland is up.", &["Hyprland"], "Hyper land is down.", "") },
    ]);
    let run = ci(&evals, &main, &[]);
    run.success();
    assert!(run.stdout().contains("1 cases in both baselines, 1 new"), "{}", run.stdout());
}

#[test]
fn cases_removed_from_the_set_are_listed_but_not_gated() {
    let (evals, main) = main_with(&[
        case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE),
        case("en-2", REFERENCE, &["psst"], REFERENCE, REFERENCE),
    ]);
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    let run = ci(&evals, &main, &[]);
    run.success();
    assert!(run.stdout().contains("only on main: en-2"), "{}", run.stdout());
}

#[test]
fn the_baseline_change_label_lets_regressions_pass() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], "Push the fax to pst before lunch.", REFERENCE)]);
    let run = ci(&evals, &main, &["needs review", "baseline-change"]);
    run.success();
    assert!(run.stdout().contains("the `baseline-change` label lets 2 regression(s) pass"), "{}", run.stdout());
}

#[test]
fn other_labels_dont_waive_anything() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], "Push the fax to psst before lunch.", REFERENCE)]);
    ci(&evals, &main, &["baseline"]).fails_with("WER rose");
}

#[test]
fn a_committed_baseline_that_differs_from_the_replay_fails_the_check() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.write_baseline();
    evals.write("baseline.json", &evals.read("baseline.json").replace("before lunch", "after lunch"));
    evals.run("replay", &["--check"]).fails_with(
        "baseline.json doesn't match this replay (cases en-1 differ); run `cargo run -p psst-eval -- replay --write`",
    );
}

#[test]
fn the_label_never_waives_an_inconsistent_baseline() {
    let (evals, main) = main_with(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], "Push the fax to psst before lunch.", REFERENCE)]);
    let event = evals.event(&["baseline-change"]);
    // Traces changed, but baseline.json wasn't regenerated.
    evals.run("replay", &["--check", "--against", &main, "--event", &event]).fails_with("doesn't match this replay");
}

#[test]
fn a_missing_baseline_fails_the_check_once_traces_are_recorded() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.run("replay", &["--check"]).fails_with("baseline.json is missing");
}

#[test]
fn an_eval_set_without_traces_passes_with_no_baseline() {
    let evals = Evals::new();
    evals.set_cases(&[Case { engine_output: None, engine_output_raw: None, ..case("en-1", REFERENCE, &[], "", "") }]);
    let run = evals.run("replay", &["--check"]);
    run.success();
    assert!(run.stdout().contains("1 of 1 cases have no traces yet"), "{}", run.stdout());
}

#[test]
fn a_changed_profile_style_makes_the_traces_stale() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.write_baseline();
    evals.write("profiles.toml", &common::PROFILES_TOML.replace("the NixOS config", "my NixOS config"));
    evals
        .run("replay", &["--write", "--check"])
        .fails_with("stale trace, case en-1 engine_output: recorded with prompt");
}

#[test]
fn a_new_term_that_fits_the_prompt_makes_the_traces_stale() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.write("dictionary.toml", &format!("{}\n[[terms]]\ntext = \"Zellij\"\n", common::DICTIONARY_TOML));
    evals
        .run("replay", &["--write"])
        .fails_with("the prompt has 3 of 4 terms, but the next one (`Zellij`) fits the budget");
}

#[test]
fn a_changed_engine_option_makes_the_traces_stale() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.write("server.toml", &common::SERVER_TOML.replace("beam_size = 5", "beam_size = 3"));
    evals.run("replay", &["--write"]).fails_with("option `beam_size` is 5 in the trace, 3 now");
}

#[test]
fn a_changed_model_makes_the_traces_stale() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)]);
    evals.write("server.toml", &common::SERVER_TOML.replace("model = \"large-v3-turbo\"", "model = \"large-v3\""));
    evals.run("replay", &["--write"]).fails_with("recorded with model `large-v3-turbo`, configured `large-v3`");
}

#[test]
fn a_raw_trace_recorded_with_a_prompt_is_stale() {
    let evals = Evals::new();
    evals.set_cases(&[Case {
        engine_output_raw: Some(trace("en", common::EN_PROMPT, REFERENCE)),
        ..case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE)
    }]);
    evals.run("replay", &["--write"]).fails_with("case en-1 engine_output_raw: recorded with prompt");
}

#[test]
fn traces_with_options_the_engine_added_are_current() {
    let evals = Evals::new();
    let mut added = case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE);
    for trace in [added.engine_output.as_mut().unwrap(), added.engine_output_raw.as_mut().unwrap()] {
        trace.options["chunk_length"] = 30.into();
    }
    evals.set_cases(&[added]);
    evals.run("replay", &["--write"]).success();
}

#[test]
fn each_language_uses_its_profile() {
    let evals = Evals::new();
    let czech = "Pushni opravu do psst.";
    evals.set_cases(&[Case {
        lang: "cs".into(),
        engine_output: Some(trace("cs", CS_PROMPT, czech)),
        engine_output_raw: Some(trace("cs", "", czech)),
        ..case("cs-1", czech, &["psst"], czech, czech)
    }]);
    let run = evals.run("replay", &["--write"]);
    run.success();
    assert!(run.stdout().contains("synthetic/cs/tune"), "{}", run.stdout());
}

/// With the style, four of these 40-byte terms fit in 223 bytes, five don't.
fn long_terms() -> (Vec<String>, String) {
    let terms: Vec<String> = (0..8).map(|i| format!("Term{i}{}", "x".repeat(35))).collect();
    let toml = terms.iter().map(|t| format!("[[terms]]\ntext = \"{t}\"\n")).collect::<Vec<_>>().join("\n");
    (terms, toml)
}

#[test]
fn terms_dropped_to_fit_the_budget_are_current() {
    let evals = Evals::new();
    let (terms, toml) = long_terms();
    evals.write("dictionary.toml", &toml);
    let prompt = format!("I'm updating the NixOS config. {}.", terms[..4].join(", "));
    evals.set_cases(&[Case {
        engine_output: Some(trace("en", &prompt, REFERENCE)),
        ..case("en-1", REFERENCE, &[], REFERENCE, REFERENCE)
    }]);
    evals.run("replay", &["--write"]).success();
}

#[test]
fn terms_dropped_although_they_fit_are_stale() {
    let evals = Evals::new();
    let (terms, toml) = long_terms();
    evals.write("dictionary.toml", &toml);
    let prompt = format!("I'm updating the NixOS config. {}.", terms[..3].join(", "));
    evals.set_cases(&[Case {
        engine_output: Some(trace("en", &prompt, REFERENCE)),
        ..case("en-1", REFERENCE, &[], REFERENCE, REFERENCE)
    }]);
    evals.run("replay", &["--write"]).fails_with("the prompt has 3 of 8 terms");
}

#[test]
fn floors_are_reported_and_enforced_only_on_request() {
    let evals = Evals::new();
    // The dictionary made the held-out output worse than raw.
    evals.set_cases(&[Case {
        split: Split::Holdout,
        ..case("en-1", REFERENCE, &["psst"], "Push the fax to psst before lunch.", REFERENCE)
    }]);
    let run = evals.run("replay", &["--write"]);
    run.success();
    assert!(run.stdout().contains("floor (not enforced yet): synthetic/en/holdout: WER is 14.29%"), "{}", run.stdout());
    evals.run("replay", &["--floors"]).fails_with("floor: synthetic/en/holdout: WER is 14.29%");
}

#[test]
fn a_listed_term_missing_from_the_reference_is_an_error() {
    let evals = Evals::new();
    evals.set_cases(&[case("en-1", REFERENCE, &["Hyprland"], REFERENCE, REFERENCE)]);
    let run = evals.run("replay", &["--write"]);
    assert_eq!(run.code(), 2, "{}", run.stderr());
    assert!(run.stderr().contains("case en-1: term `Hyprland` is listed but doesn't occur in the reference"));
}

#[test]
fn a_case_with_only_one_trace_is_an_error() {
    let evals = Evals::new();
    evals.set_cases(&[Case { engine_output_raw: None, ..case("en-1", REFERENCE, &["psst"], REFERENCE, REFERENCE) }]);
    let run = evals.run("replay", &[]);
    assert_eq!(run.code(), 2, "{}", run.stderr());
    assert!(run.stderr().contains("case en-1 has only one of engine_output and engine_output_raw"));
}
