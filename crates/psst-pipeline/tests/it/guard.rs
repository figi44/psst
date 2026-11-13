//! The hallucination guard.

use crate::support;

use support::*;

#[test]
fn no_speech_from_vad_empties_the_result() {
    let out = run_with(&trace("en", Some(0), vec![segment(" Thanks for watching!"), segment(" Bye.")]), "");
    assert_eq!(out.text, "");
    assert_eq!(out.raw_text, "Thanks for watching! Bye.");
    assert_eq!(out.edits, [guard("no_speech", 0..20, "Thanks for watching!"), guard("no_speech", 20..25, " Bye.")]);
}

#[test]
fn a_known_phrase_with_a_high_no_speech_prob_is_dropped() {
    let out = run_with(
        &trace("en", Some(4000), vec![segment_p(" Push the fix.", 0.1), segment_p(" Thanks for watching!", 0.8)]),
        "",
    );
    assert_eq!(out.text, "Push the fix. ");
    assert_eq!(out.edits[0], guard("hallucination", 13..34, " Thanks for watching!"));
}

#[test]
fn a_dropped_first_segment_takes_the_space_after_it() {
    let out =
        run_with(&trace("en", Some(4000), vec![segment_p(" Thank you.", 0.9), segment_p(" Deploy it.", 0.1)]), "");
    assert_eq!(out.text, "Deploy it. ");
    assert_eq!(out.edits[0], guard("hallucination", 0..11, "Thank you. "));
}

#[test]
fn a_dropped_middle_segment_leaves_single_spacing() {
    let segments = vec![segment_p(" One.", 0.1), segment_p(" Thank you.", 0.9), segment_p(" Two.", 0.1)];
    let out = run_with(&trace("en", Some(4000), segments), "");
    assert_eq!(out.text, "One. Two. ");
}

#[test]
fn known_phrases_are_matched_ignoring_case_and_punctuation() {
    for phrase in [
        " thank you",
        " THANK YOU!!",
        " Thank you...",
        " Subtitles by the Amara.org community",
        " I’ll see you next time.",
    ] {
        assert_eq!(text(&trace("en", Some(4000), vec![segment_p(phrase, 0.9)]), ""), "", "{phrase}");
    }
}

#[test]
fn a_known_phrase_with_strong_speech_evidence_survives() {
    assert_eq!(text(&trace("en", Some(4000), vec![segment_p(" Thank you.", 0.3)]), ""), "Thank you. ");
    assert_eq!(text(&trace("en", Some(4000), vec![segment_p(" Thank you.", 0.6)]), ""), "Thank you. ");
}

#[test]
fn without_a_probability_less_than_500_ms_of_speech_is_weak_evidence() {
    let thanks = |speech_ms| text(&trace("en", speech_ms, vec![segment(" Thank you.")]), "");
    assert_eq!(thanks(Some(499)), "");
    assert_eq!(thanks(Some(500)), "Thank you. ");
    assert_eq!(thanks(None), "Thank you. ");
}

#[test]
fn short_real_dictations_survive_weak_speech_evidence() {
    for word in [" OK", " OK.", " no", " No.", " Hyprland"] {
        assert!(!text(&trace("en", Some(300), vec![segment_p(word, 0.95)]), "").is_empty(), "{word}");
        assert!(!text(&trace("en", Some(300), vec![segment(word)]), "").is_empty(), "{word}");
    }
}

#[test]
fn a_phrase_is_only_dropped_as_a_whole_segment() {
    let out = text(&trace("en", Some(300), vec![segment_p(" Thank you for the review, merging now.", 0.9)]), "");
    assert_eq!(out, "Thank you for the review, merging now. ");
}

#[test]
fn phrases_are_per_language() {
    let johny = |lang| text(&trace(lang, Some(300), vec![segment_p(" Titulky vytvořil JohnyX", 0.9)]), "");
    assert_eq!(johny("cs"), "");
    assert_eq!(johny("en"), "Titulky vytvořil JohnyX ");
    assert_eq!(
        text(&trace("cs", Some(300), vec![segment_p(" Thanks for watching!", 0.9)]), ""),
        "Thanks for watching! "
    );
}

#[test]
fn the_dictionary_never_touches_a_dropped_segment() {
    let out = run_with(
        &trace(
            "en",
            Some(4000),
            vec![segment_p(" Thanks for watching!", 0.9), segment_p(" Thanks for watching, she said.", 0.1)],
        ),
        "[[terms]]\ntext = \"TFW\"\nsounds_like = [\"thanks for watching\"]",
    );
    assert_eq!(out.text, "TFW, she said. ");
    assert_eq!(out.edits.iter().filter(|e| e.term.is_some()).count(), 1);
}
