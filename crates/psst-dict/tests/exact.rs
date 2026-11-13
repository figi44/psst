//! The exact-spelling matcher (plan 7.2 step 2) and the common-word lists.

use psst_config::{ConfigFile, DictionaryFile};
use psst_dict::{Dictionary, Outcome, common_words, is_common_word};

fn dictionary(toml: &str) -> Dictionary {
    Dictionary::from_file(&DictionaryFile::from_toml_str(toml).unwrap()).unwrap()
}

/// Each match as (matched text, term, outcome).
fn matches<'t>(dict: &Dictionary, text: &'t str, lang: &str) -> Vec<(&'t str, String, Outcome)> {
    dict.exact_matches(text, lang, |_| true)
        .into_iter()
        .map(|m| (&text[m.range], m.term.text.clone(), m.outcome))
        .collect()
}

fn found(matched: &str, term: &str, outcome: Outcome) -> (String, String, Outcome) {
    (matched.into(), term.into(), outcome)
}

fn owned(found: Vec<(&str, String, Outcome)>) -> Vec<(String, String, Outcome)> {
    found.into_iter().map(|(m, t, o)| (m.to_owned(), t, o)).collect()
}

const LANGUAGES_C: &str = "[[terms]]\ntext = \"C\"\n[[terms]]\ntext = \"C++\"\n[[terms]]\ntext = \"C#\"";

#[test]
fn the_longest_spelling_wins_so_cpp_and_csharp_never_match_c() {
    let dict = dictionary(LANGUAGES_C);
    assert_eq!(
        owned(matches(&dict, "C++, C# and C.", "en")),
        [
            found("C++", "C++", Outcome::Keep),
            found("C#", "C#", Outcome::Keep),
            // "c" is a common English word.
            found("C", "C", Outcome::Ignore),
        ]
    );
    assert_eq!(
        owned(matches(&dict, "c++ or c#", "de")),
        [found("c++", "C++", Outcome::Replace), found("c#", "C#", Outcome::Replace)]
    );
}

#[test]
fn plus_hash_hyphen_and_slash_are_not_boundaries() {
    let dict = dictionary("[[terms]]\ntext = \"C\"\n[[terms]]\ntext = \"Rust\"");
    for text in ["c+", "c+++", "c#x", "rust-analyzer", "c/c", "rustc", "trust", "#rust", "+rust"] {
        assert!(matches(&dict, text, "de").is_empty(), "{text}");
    }
}

#[test]
fn whitespace_text_edges_and_the_listed_punctuation_are_boundaries() {
    let dict = dictionary("[[terms]]\ntext = \"Hyprland\"");
    for text in [
        "hyprland",
        " hyprland ",
        "\thyprland\n",
        "hyprland.",
        "hyprland,",
        "hyprland;",
        "hyprland:",
        "hyprland!",
        "hyprland?",
        "(hyprland)",
        "[hyprland]",
        "{hyprland}",
        "\"hyprland\"",
        "'hyprland'",
        "hyprland's",
        "‘hyprland’",
        "“hyprland”",
        "„hyprland“",
        "hyprland…",
        "...hyprland...",
        "!hyprland?",
    ] {
        assert_eq!(owned(matches(&dict, text, "en")), [found("hyprland", "Hyprland", Outcome::Replace)], "{text:?}");
    }
}

#[test]
fn a_spelling_may_start_or_end_with_punctuation() {
    let dict =
        dictionary("[[terms]]\ntext = \".NET\"\n[[terms]]\ntext = \"O'Reilly\"\nsounds_like = ['\"the publisher\"']");
    assert_eq!(
        owned(matches(&dict, "learn .net from \"the publisher\" or the publisher, not asp.net", "en")),
        [found(".net", ".NET", Outcome::Replace), found("\"the publisher\"", "O'Reilly", Outcome::Replace)]
    );
}

#[test]
fn matching_ignores_case_beyond_ascii_and_keeps_offsets_right() {
    let dict = dictionary(
        "[[terms]]\ntext = \"Šťastný\"\n[[terms]]\ntext = \"Straße\"\n[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]",
    );
    // `İ` folds to two characters, `ẞ` to `ss`: matches after them still map back exactly.
    assert_eq!(
        owned(matches(&dict, "İİ ŠŤASTNÝ GROẞE STRASSE, Hyper Land", "de")),
        [
            found("ŠŤASTNÝ", "Šťastný", Outcome::Replace),
            found("STRASSE", "Straße", Outcome::Replace),
            found("Hyper Land", "Hyprland", Outcome::Replace),
        ]
    );
}

#[test]
fn a_shorter_or_later_match_takes_over_from_one_that_fails_its_boundaries() {
    let dict = dictionary("[[terms]]\ntext = \"Qux Blorp\"\n[[terms]]\ntext = \"Blorp\"\n[[terms]]\ntext = \"Qux\"");
    assert_eq!(owned(matches(&dict, "aqux blorp", "en")), [found("blorp", "Blorp", Outcome::Replace)]);
    assert_eq!(owned(matches(&dict, "qux blorpz", "en")), [found("qux", "Qux", Outcome::Replace)]);
    assert_eq!(owned(matches(&dict, "qux blorp", "en")), [found("qux blorp", "Qux Blorp", Outcome::Replace)]);
}

#[test]
fn a_vetoed_candidate_leaves_room_for_the_next_best() {
    let dict = dictionary("[[terms]]\ntext = \"Claude Code\"\n[[terms]]\ntext = \"Claude\"");
    let text = "claude code";
    let ranges: Vec<_> =
        dict.exact_matches(text, "en", |range| range != (0..11)).into_iter().map(|m| m.range).collect();
    assert_eq!(ranges, vec![(0..6)]);
}

#[test]
fn only_terms_for_the_language_match() {
    let dict = dictionary(
        "[[terms]]\ntext = \"Dvořák\"\nlanguages = \"cs\"\n[[terms]]\ntext = \"NixOS\"\nlanguages = \"en\"\n[[terms]]\ntext = \"psst\"",
    );
    let text = "dvořák nixos PSST";
    assert_eq!(
        owned(matches(&dict, text, "cs")),
        [found("dvořák", "Dvořák", Outcome::Replace), found("PSST", "psst", Outcome::Replace)]
    );
    assert_eq!(
        owned(matches(&dict, text, "en")),
        [found("nixos", "NixOS", Outcome::Replace), found("PSST", "psst", Outcome::Replace)]
    );
}

#[test]
fn case_is_fixed_unless_the_text_is_a_common_word() {
    let dict = dictionary("[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"Go\"\n[[terms]]\ntext = \"psst\"");
    assert_eq!(
        owned(matches(&dict, "gpu GPU go Go GO psst", "en")),
        [
            found("gpu", "GPU", Outcome::Replace),
            found("GPU", "GPU", Outcome::Keep),
            found("go", "Go", Outcome::Ignore),
            found("Go", "Go", Outcome::Ignore),
            found("GO", "Go", Outcome::Ignore),
            found("psst", "psst", Outcome::Keep),
        ]
    );
    // Czech has no "go", so there the case is fixed.
    assert_eq!(owned(matches(&dict, "go", "cs")), [found("go", "Go", Outcome::Replace)]);
}

#[test]
fn a_sounds_like_that_is_a_common_word_is_still_replaced() {
    let dict = dictionary("[[terms]]\ntext = \"Nix\"\nsounds_like = [\"next\"]");
    assert_eq!(owned(matches(&dict, "next", "en")), [found("next", "Nix", Outcome::Replace)]);
}

#[test]
fn common_words_are_the_5000_most_frequent_forms_per_language() {
    assert_eq!(common_words("en").len(), 5000);
    assert_eq!(common_words("cs").len(), 5000);
    assert_eq!(common_words("en")[0], "the");
    assert!(common_words("de").is_empty());
    for (lang, word) in [("en", "The"), ("en", "GO"), ("en", "don't"), ("en", "Don’t"), ("cs", "že"), ("cs", "JE")] {
        assert!(is_common_word(lang, word), "{lang} {word}");
    }
    for (lang, word) in [("en", "Hyprland"), ("en", "go home"), ("en", ""), ("cs", "which"), ("de", "der")] {
        assert!(!is_common_word(lang, word), "{lang} {word}");
    }
}
