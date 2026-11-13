//! The built-in rules (plan 8).
//!
//! Cases marked "Handy" are adapted from Handy's `src-tauri/src/audio_toolkit/text.rs`
//! (github.com/cjpais/Handy, MIT) and cases marked "t3code" from t3code's
//! `apps/server/src/speech/SpeechService.test.ts` (github.com/pingdotgg/t3code, MIT), to psst's
//! rules: only the first letter of the text is capitalised, and only listed fillers go.

use crate::support;

use psst_config::Rule;
use psst_dict::Dictionary;
use psst_pipeline::{PipelineOptions, run};
use support::*;

fn with(profile: &psst_config::Profile, said: &str) -> String {
    let trace = trace(&profile.language, Some(3000), vec![segment(&format!(" {said}"))]);
    run(&trace, profile, &Dictionary::default(), &PipelineOptions::default()).text
}

#[test]
fn a_filler_goes_with_the_one_comma_attached_to_it() {
    for (spoken, expected) in [
        ("I, um, think", "I, think "),
        ("So um, I think", "So I think "),
        // Handy
        ("So uh I was thinking uh about this", "So I was thinking about this "),
        ("UH this is UH a test", "This is a test "),
        ("Well, erm, I think, uh, that's right", "Well, I think, that's right "),
        ("  Uh, so I was, uh, thinking about this  ", "So I was, thinking about this "),
        ("Um, so I think we should ship it.", "So I think we should ship it. "),
        // t3code
        ("Uh, Make it red, um, blue.", "Make it red, blue. "),
    ] {
        assert_eq!(text(&said(spoken), ""), expected, "{spoken}");
    }
}

#[test]
fn only_standalone_fillers_go() {
    for spoken in ["Umbrella, drum, uh-huh.", "I think, um.", "Maybe (um) later", "Thumb up"] {
        let out = text(&said(spoken), "");
        assert_eq!(out.trim_end(), spoken, "{spoken}");
    }
}

#[test]
fn fillers_come_from_the_profile() {
    // Handy: "mm" isn't a filler, and Portuguese has no list, so its "um" ("a") stays.
    assert_eq!(text(&said("the screw is 5 mm long"), ""), "The screw is 5 mm long ");
    assert_eq!(with(&profile("pt"), "um gato bonito"), "Um gato bonito ");
    // Handy: a custom list, and an empty one.
    let rules = [Rule::Fillers, Rule::Whitespace, Rule::Capitalize];
    assert_eq!(
        with(&profile_with("en", &rules, &["okay", "right"]), "okay so I think right this works"),
        "So I think this works"
    );
    assert_eq!(with(&profile_with("en", &rules, &[]), "So uh I was thinking"), "So uh I was thinking");
    // Czech fillers in a Czech session.
    assert_eq!(text(&said_cs("no ehm, já nevím, eee"), ""), "No já nevím, ");
}

#[test]
fn a_protected_filler_stays() {
    // t3code: a dictionary term for the filler keeps it.
    assert_eq!(text(&said("Make it red, um, blue."), "[[terms]]\ntext = \"um\""), "Make it red, um, blue. ");
}

#[test]
fn whitespace_collapses_runs_of_spaces_and_nothing_else() {
    // Handy
    assert_eq!(text(&said("Hello    world   test"), ""), "Hello world test ");
    // Spaces before punctuation are kept, and so are tabs and newlines.
    assert_eq!(text(&said("Wait , what ?"), ""), "Wait , what ? ");
    assert_eq!(text(&said("one\n\ntwo\t three"), ""), "One\n\ntwo\t three ");
    assert_eq!(text(&said("then cd .. twice"), ""), "Then cd .. twice ");
}

#[test]
fn whitespace_edits_cover_only_the_spaces_they_remove() {
    let out = run_with(&said("I, um, think"), "");
    assert_eq!(out.raw_text, "I, um, think");
    assert_eq!(
        out.edits,
        [rule("fillers", 3..6, "um,", ""), rule("whitespace", 2..3, " ", ""), rule("trailing_space", 12..12, "", " ")]
    );
}

#[test]
fn removing_every_token_leaves_empty_text() {
    let out = run_with(&said("um, uh"), "");
    assert_eq!(out.text, "");
    assert!(out.edits.iter().all(|e| e.rule != "trailing_space"));
}

#[test]
fn capitalize_uppercases_the_first_letter() {
    for (spoken, expected) in [
        ("hello there", "Hello there "),
        ("\"hello,\" she said", "\"Hello,\" she said "),
        ("... and then", "... And then "),
        ("3 apples", "3 apples "),
        ("ńext", "Ńext "),
        (".NET is fine", ".NET is fine "),
    ] {
        assert_eq!(text(&said(spoken), ""), expected, "{spoken}");
    }
    assert_eq!(text(&said_cs("šťastný den"), ""), "Šťastný den ");
}

#[test]
fn capitalize_skips_a_protected_first_token() {
    assert_eq!(text(&said("t3code rocks"), "[[terms]]\ntext = \"t3code\""), "t3code rocks ");
}

#[test]
fn rules_run_in_the_profiles_order() {
    // Capitalised first, the filler then goes, as one edit; without `whitespace` its spacing stays.
    let profile = profile_with("en", &[Rule::Capitalize, Rule::Fillers, Rule::TrailingSpace], &["um"]);
    assert_eq!(with(&profile, "um so"), " so ");
    let out = run(&said("um so"), &profile, &Dictionary::default(), &PipelineOptions::default());
    assert_eq!(out.edits, [rule("fillers", 0..2, "um", ""), rule("trailing_space", 5..5, "", " ")]);
    // `whitespace` after `trailing_space` removes the appended space too.
    let profile = profile_with("en", &[Rule::TrailingSpace, Rule::Whitespace], &[]);
    assert_eq!(with(&profile, "so  be it"), "so be it");
    let profile = profile_with("en", &[Rule::TrailingSpace, Rule::Fillers, Rule::Whitespace], &["um"]);
    assert_eq!(with(&profile, "um"), "");
}
