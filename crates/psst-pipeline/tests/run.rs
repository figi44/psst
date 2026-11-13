//! `raw_text`, assembly, edits and options (plan 6.4 steps 1, 2 and 6).

mod support;

use psst_config::ProfilesFile;
use psst_dict::Dictionary;
use psst_pipeline::{PipelineOptions, run};
use psst_proto::{Final, Word};
use support::*;

#[test]
fn raw_text_is_the_trimmed_segments_joined_by_single_spaces() {
    let out = run_with(
        &trace("cs", Some(3000), vec![segment(" Push the fix."), segment("  "), segment(" Then  deploy. ")]),
        "",
    );
    assert_eq!(out.raw_text, "Push the fix. Then  deploy.");
    assert_eq!(out.text, "Push the fix. Then deploy. ");
    assert_eq!(out.language, "cs");
}

#[test]
fn every_change_is_an_edit_with_its_raw_span_in_pipeline_order() {
    let out =
        run_with(&said("Um, hyper land is great."), "[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]");
    assert_eq!(
        out,
        Final {
            text: "Hyprland is great. ".into(),
            raw_text: "Um, hyper land is great.".into(),
            edits: vec![
                exact(4..14, "hyprland", "hyper land", "Hyprland"),
                rule("fillers", 0..3, "Um,", ""),
                rule("whitespace", 3..4, " ", ""),
                // A protected first token isn't capitalised.
                rule("trailing_space", 24..24, "", " "),
            ],
            language: "en".into(),
        }
    );
}

#[test]
fn without_the_dictionary_option_only_the_guard_and_rules_run() {
    let trace = said("hyper land is great.");
    let dict = dictionary("[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]");
    let out = run(&trace, &profile("en"), &dict, &PipelineOptions { dictionary: false });
    assert_eq!(out.text, "Hyper land is great. ");
    assert!(out.edits.iter().all(|e| e.term.is_none()));
}

#[test]
fn a_profile_without_rules_returns_raw_text() {
    let profile = profile_with("en", &[], &["um"]);
    let out = run(&said("um  hello there"), &profile, &Dictionary::default(), &PipelineOptions::default());
    assert_eq!(out.text, out.raw_text);
    assert!(out.edits.is_empty());
}

#[test]
fn no_segments_or_only_blank_ones_give_empty_text() {
    for segments in [vec![], vec![segment(""), segment("   ")]] {
        let out = run_with(&trace("en", Some(3000), segments), "");
        assert_eq!((out.text.as_str(), out.raw_text.as_str()), ("", ""));
        assert!(out.edits.is_empty());
    }
}

#[test]
fn the_text_is_never_rebuilt_from_word_items() {
    // Word items split at punctuation ("3" + ".14"), or disagree with the segment text: either
    // way the segment text is what comes out.
    let word = |text: &str, prob| Word { text: text.into(), start_ms: 0, end_ms: 0, prob };
    for words in [
        vec![word(" Pi", 0.9), word(" is", 0.9), word(" 3", 0.9), word(".14.", 0.2)],
        vec![word(" Pie", 0.9), word(" is", 0.9), word(" 3", 0.9), word(" .14", 0.9)],
        vec![],
    ] {
        let mut segment = segment(" Pi is 3.14.");
        segment.words = Some(words);
        assert_eq!(text(&trace("en", Some(3000), vec![segment]), ""), "Pi is 3.14. ");
    }
}

#[test]
fn the_profiles_language_applies_when_the_engine_reports_none() {
    let dict = dictionary("[[terms]]\ntext = \"Dvořák\"\nlanguages = \"cs\"");
    let file = ProfilesFile::default();
    let mut czech = file.resolve("default").unwrap();
    czech.language = "cs".into();
    let out = run(&trace("", Some(3000), vec![segment(" dvořák")]), &czech, &dict, &PipelineOptions::default());
    assert_eq!(out.text, "Dvořák ");
}
