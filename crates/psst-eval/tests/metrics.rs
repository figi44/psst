//! The metric definitions of plan 7.3.

use psst_eval::{
    metrics::{Counts, Scoring, score},
    text::{count_term, normalise},
};
use psst_proto::{Edit, Final, Stage};

fn fillers() -> Vec<String> {
    vec!["um".into(), "uh".into()]
}

fn edit(stage: Stage, rule: &str, raw: &str, span: &str, after: &str) -> Edit {
    let start = raw.find(span).expect("span in raw text");
    Edit {
        stage,
        rule: rule.into(),
        term: None,
        raw_span: start..start + span.len(),
        before: span.into(),
        after: after.into(),
        score: None,
    }
}

fn output(raw: &str, edits: Vec<Edit>, text: &str) -> Final {
    Final { text: text.into(), raw_text: raw.into(), edits, language: "en".into() }
}

fn counts(reference: &str, terms: &[&str], out: &Final, every_dictionary_edit_counts: bool) -> Counts {
    let terms: Vec<String> = terms.iter().map(|t| t.to_string()).collect();
    let fillers = fillers();
    score(&Scoring { reference, terms: &terms, fillers: &fillers, every_dictionary_edit_counts }, out).unwrap()
}

#[test]
fn normalisation_lowercases_and_strips_punctuation_but_keeps_words_whole() {
    assert_eq!(
        normalise("“Don’t” — STOP, um, the large-v3-turbo model's -- end!", &fillers()),
        ["don't", "stop", "the", "large-v3-turbo", "model's", "end"]
    );
}

#[test]
fn normalisation_composes_unicode_and_removes_fillers() {
    assert_eq!(normalise("Dvor\u{030C}a\u{0301}k uh Dvořák", &fillers()), ["dvořák", "dvořák"]);
}

#[test]
fn terms_count_only_with_their_exact_spelling_at_word_edges() {
    assert_eq!(count_term("Use C++ and C# but C.", "C"), 1);
    assert_eq!(count_term("C++ beats C, says C++.", "C++"), 2);
    assert_eq!(count_term("kubernetes or Kubernetes's pods", "Kubernetes"), 1);
    assert_eq!(count_term("t3codes and (t3code)", "t3code"), 1);
    assert_eq!(count_term("Claude Code or claude code", "Claude Code"), 1);
}

#[test]
fn term_recall_and_wer_compare_with_the_reference() {
    let raw = "push the fix to pst";
    let c = counts("Push the fix to psst.", &["psst"], &output(raw, vec![], raw), false);
    assert_eq!((c.term_occurrences, c.term_hits), (1, 0));
    assert_eq!((c.ref_words, c.word_errors, c.output_words), (5, 1, 5));
    assert_eq!(c.term_recall(), Some(0.0));
    assert_eq!(c.wer(), Some(20.0));
}

#[test]
fn repeated_terms_count_every_occurrence() {
    let raw = "psst here and pst there";
    let c = counts("psst here and psst there", &["psst"], &output(raw, vec![], raw), false);
    assert_eq!((c.term_occurrences, c.term_hits), (2, 1));
}

#[test]
fn a_dictionary_edit_that_breaks_a_correct_word_is_a_false_correction() {
    let raw = "I like the cube.";
    let out = output(raw, vec![edit(Stage::Dictionary, "fuzzy", raw, "cube", "Kube")], "I like the Kube.");
    assert_eq!(counts("I like the cube.", &[], &out, false).false_corrections, 1);
}

#[test]
fn an_edit_counts_when_it_unmatches_a_correct_word_whatever_else_it_fixes() {
    let raw = "cloud cod";
    let out = output(raw, vec![edit(Stage::Dictionary, "fuzzy", raw, "cloud cod", "Claude Code")], "Claude Code");
    assert_eq!(counts("cloud code", &[], &out, false).false_corrections, 1);
}

#[test]
fn an_edit_that_keeps_a_correct_word_is_not_whatever_the_alignment_does_with_repeats() {
    let raw = "Use cloud Claude cod you works with psst.";
    let out = output(
        raw,
        vec![edit(Stage::Dictionary, "fuzzy", raw, "Claude cod", "Claude Code")],
        "Use cloud Claude Code you works with psst.",
    );
    let c = counts("Use Claude. Claude Code works with psst.", &["Claude Code"], &out, false);
    assert_eq!(c.false_corrections, 0);
}

#[test]
fn removing_a_repeated_word_keeps_the_correct_one() {
    let raw = "Use Claude Claude.";
    let out = output(raw, vec![edit(Stage::Rules, "fillers", raw, " Claude.", ".")], "Use Claude.");
    assert_eq!(counts("Use Claude Code.", &[], &out, true).false_corrections, 0);
}

#[test]
fn an_edit_that_adds_missing_words_to_a_correct_one_is_not() {
    let raw = "Claude is great";
    let out = output(raw, vec![edit(Stage::Dictionary, "fuzzy", raw, "Claude", "Claude Code")], "Claude Code is great");
    let c = counts("Claude Code is great", &["Claude Code"], &out, false);
    assert_eq!((c.false_corrections, c.word_errors), (0, 0));
}

#[test]
fn a_dictionary_edit_that_fixes_a_term_is_not() {
    let raw = "Run hyper land now.";
    let out = output(raw, vec![edit(Stage::Dictionary, "exact", raw, "hyper land", "Hyprland")], "Run Hyprland now.");
    let c = counts("Run Hyprland now.", &["Hyprland"], &out, false);
    assert_eq!((c.false_corrections, c.term_hits, c.word_errors), (0, 1, 0));
}

#[test]
fn a_case_fix_changes_no_words() {
    let raw = "the gpu is hot";
    let out = output(raw, vec![edit(Stage::Dictionary, "exact", raw, "gpu", "GPU")], "the GPU is hot");
    assert_eq!(counts("The GPU is hot.", &["GPU"], &out, false).false_corrections, 0);
}

#[test]
fn on_clips_without_terms_every_dictionary_edit_counts() {
    let raw = "the gpu is hot";
    let out = output(raw, vec![edit(Stage::Dictionary, "exact", raw, "gpu", "GPU")], "the GPU is hot");
    assert_eq!(counts("the gpu is hot", &[], &out, true).false_corrections, 1);
}

#[test]
fn a_rule_that_removes_a_correct_word_is_a_false_correction_and_a_filler_is_not() {
    let raw = "I, um, like it";
    let fillers_removed = output(raw, vec![edit(Stage::Rules, "fillers", raw, " um,", "")], "I, like it");
    assert_eq!(counts("I like it", &[], &fillers_removed, true).false_corrections, 0);
    let word_removed = output(raw, vec![edit(Stage::Rules, "fillers", raw, " like", "")], "I, um, it");
    assert_eq!(counts("I like it", &[], &word_removed, true).false_corrections, 1);
}

#[test]
fn guard_edits_are_never_false_corrections() {
    let raw = "Thanks for watching!";
    let out = output(raw, vec![edit(Stage::Guard, "hallucination", raw, raw, "")], "");
    let c = counts("Thanks for watching!", &[], &out, true);
    assert_eq!((c.false_corrections, c.word_errors, c.output_words), (0, 3, 0));
}

#[test]
fn later_edits_apply_to_the_text_earlier_ones_made() {
    let raw = "hyper land is up";
    let edits = vec![
        edit(Stage::Dictionary, "exact", raw, "hyper land", "Hyprland"),
        Edit { raw_span: 0..10, ..edit(Stage::Rules, "capitalize", raw, "hyper land", "HYPRLAND") },
        Edit { raw_span: raw.len()..raw.len(), ..edit(Stage::Rules, "trailing_space", raw, "", " ") },
    ];
    let c = counts("Hyprland is up", &["Hyprland"], &output(raw, edits, "HYPRLAND is up "), false);
    // The capitalisation changed a correct word's case only.
    assert_eq!((c.false_corrections, c.term_hits, c.word_errors), (0, 0, 0));
}

#[test]
fn edits_that_dont_give_the_final_text_are_an_error() {
    let raw = "the gpu is hot";
    let out = output(raw, vec![edit(Stage::Dictionary, "exact", raw, "gpu", "GPU")], "the GPU is cold");
    let fillers = fillers();
    let scoring = Scoring { reference: raw, terms: &[], fillers: &fillers, every_dictionary_edit_counts: true };
    let err = score(&scoring, &out).unwrap_err().to_string();
    assert!(err.contains("the edits applied to raw_text give \"the GPU is hot\""), "{err}");
}

#[test]
fn rates_are_per_1000_output_words() {
    let c = Counts { false_corrections: 1, output_words: 500, ..Counts::default() };
    assert_eq!(c.false_corrections_per_1000(), 2.0);
    assert_eq!(Counts::default().term_recall(), None);
    assert_eq!(Counts::default().wer(), None);
}
