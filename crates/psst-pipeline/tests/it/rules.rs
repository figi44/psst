//! The built-in rules: only the first letter of the text is capitalised, and only listed
//! fillers go.

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
        ("Then uh we merge it uh tomorrow", "Then we merge it tomorrow "),
        ("UM the build is UM green", "The build is green "),
        ("Okay, erm, the tests pass, uh, mostly", "Okay, the tests pass, mostly "),
        ("  Um, the deploy is, uh, finished  ", "The deploy is, finished "),
        ("Uh, let's merge it after lunch.", "Let's merge it after lunch. "),
        ("Um, Paint it green, uh, yellow.", "Paint it green, yellow. "),
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
    // "ehm" is a filler only in Czech, and German has no list, so its "um" ("at") stays.
    assert_eq!(text(&said("ehm is a Czech filler"), ""), "Ehm is a Czech filler ");
    assert_eq!(with(&profile("de"), "um acht Uhr"), "Um acht Uhr ");
    // A custom list, and an empty one.
    let rules = [Rule::Fillers, Rule::Whitespace, Rule::Capitalize];
    assert_eq!(
        with(&profile_with("en", &rules, &["basically", "like"]), "basically the fix like works"),
        "The fix works"
    );
    assert_eq!(with(&profile_with("en", &rules, &[]), "Then uh it works"), "Then uh it works");
    // Czech fillers in a Czech session.
    assert_eq!(text(&said_cs("no ehm, já nevím, eee"), ""), "No já nevím, ");
}

#[test]
fn a_protected_filler_stays() {
    // A dictionary term for the filler keeps it.
    assert_eq!(text(&said("Set it to ten, uh, twelve."), "[[terms]]\ntext = \"uh\""), "Set it to ten, uh, twelve. ");
}

#[test]
fn whitespace_collapses_runs_of_spaces_and_nothing_else() {
    assert_eq!(text(&said("Ship    it   today"), ""), "Ship it today ");
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
