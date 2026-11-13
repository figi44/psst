//! The dictionary stage with exact spellings (plan 7.2 step 2), through `run`.
//!
//! Cases marked "Handy" are adapted from Handy's `src-tauri/src/audio_toolkit/text.rs`
//! (github.com/cjpais/Handy, MIT) and cases marked "t3code" from t3code's
//! `apps/server/src/speech/SpeechService.test.ts` (github.com/pingdotgg/t3code, MIT). Their fuzzy
//! cases are in `fuzzy.rs`.

mod support;

use support::*;

const HYPRLAND: &str = "[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]";

#[test]
fn only_the_matched_characters_are_replaced_and_punctuation_stays() {
    for (spoken, expected) in [
        ("hyper land.", "Hyprland. "),
        ("I use hyper land, mostly.", "I use Hyprland, mostly. "),
        ("(hyper land)", "(Hyprland) "),
        ("\"Hyper Land\"?", "\"Hyprland\"? "),
        ("hyper land's config", "Hyprland's config "),
        ("hyper land’s config", "Hyprland’s config "),
        // Handy: punctuation around a word.
        ("!hyprland?", "!Hyprland? "),
        ("...hyprland...", "...Hyprland... "),
    ] {
        assert_eq!(text(&said(spoken), HYPRLAND), expected, "{spoken}");
    }
}

#[test]
fn a_replacement_records_exactly_the_characters_it_replaced() {
    let out = run_with(&said("so hyper land."), HYPRLAND);
    assert_eq!(out.edits[0], exact(3..13, "hyprland", "hyper land", "Hyprland"));
}

#[test]
fn c_cpp_and_csharp_match_by_boundaries_and_the_longest_spelling() {
    let dict =
        "[[terms]]\ntext = \"C\"\n[[terms]]\ntext = \"C++\"\n[[terms]]\ntext = \"C#\"\n[[terms]]\ntext = \"Rust\"";
    // "c" is a common English word, so its case isn't fixed; "c++" and "c#" are never `C`.
    assert_eq!(text(&said("I write c++, c# and c, not rust."), dict), "I write C++, C# and c, not Rust. ");
    assert_eq!(text(&said("c/c++ and c+ stay"), dict), "C/c++ and c+ stay ");
}

#[test]
fn case_is_fixed_by_the_canonical_text() {
    let out = run_with(&said("the gpu and the Gpu"), "[[terms]]\ntext = \"GPU\"");
    assert_eq!(out.text, "The GPU and the GPU ");
    assert_eq!(&out.edits[..2], [exact(4..7, "gpu", "gpu", "GPU"), exact(16..19, "gpu", "Gpu", "GPU")]);
}

#[test]
fn a_common_word_is_neither_changed_nor_protected() {
    let go = "[[terms]]\ntext = \"Go\"";
    let out = run_with(&said("go home"), go);
    assert_eq!(out.text, "Go home ");
    assert_eq!(out.edits, [rule("capitalize", 0..2, "go", "Go"), rule("trailing_space", 7..7, "", " ")]);
    assert_eq!(text(&said("use go modules"), go), "Use go modules ");
    // Handy: "world" is a common word, "hello" isn't.
    assert_eq!(text(&said("hello world"), "[[terms]]\ntext = \"Hello\"\n[[terms]]\ntext = \"World\""), "Hello world ");
}

#[test]
fn a_match_that_changes_nothing_is_protected_without_an_edit() {
    let out = run_with(&said("psst is quiet"), "[[terms]]\ntext = \"psst\"");
    assert_eq!(out.text, "psst is quiet ");
    assert_eq!(out.edits, [rule("trailing_space", 13..13, "", " ")]);
    // Handy: R&D is matched whole, since `&` isn't a boundary.
    let out = run_with(&said("send it to R&D for review"), "[[terms]]\ntext = \"R&D\"\n[[terms]]\ntext = \"D\"");
    assert_eq!(out.text, "Send it to R&D for review ");
}

#[test]
fn a_replaced_first_token_keeps_its_brand_casing() {
    assert_eq!(
        text(&said("T3 code is open."), "[[terms]]\ntext = \"t3code\"\nsounds_like = [\"t3 code\"]"),
        "t3code is open. "
    );
}

#[test]
fn the_longest_match_at_the_leftmost_position_wins() {
    let dict = "[[terms]]\ntext = \"Claude Code\"\n[[terms]]\ntext = \"Claude\"";
    assert_eq!(text(&said("claude code and claude"), dict), "Claude Code and Claude ");
}

#[test]
fn only_terms_for_the_session_language_apply() {
    let dict = "[[terms]]\ntext = \"Dvořák\"\nlanguages = \"cs\"\n[[terms]]\ntext = \"NixOS\"\nlanguages = \"en\"\nsounds_like = [\"nix os\"]";
    assert_eq!(text(&said("I like dvořák on nix os"), dict), "I like dvořák on NixOS ");
    assert_eq!(text(&said_cs("hraju dvořák na nix os"), dict), "Hraju Dvořák na nix os ");
}

#[test]
fn matching_ignores_case_beyond_ascii() {
    let dict = "[[terms]]\ntext = \"Šťastný\"\nsounds_like = [\"stastny\"]";
    assert_eq!(text(&said_cs("pan ŠŤASTNÝ a pan Stastny"), dict), "Pan Šťastný a pan Šťastný ");
}

#[test]
fn dot_net_and_cd_dot_dot_are_untouched() {
    assert_eq!(text(&said("I deploy to .NET, then cd .. twice."), ""), "I deploy to .NET, then cd .. twice. ");
    let dict = "[[terms]]\ntext = \".NET\"\n[[terms]]\ntext = \"NET\"";
    assert_eq!(text(&said("learn .net, not asp.net or cd .."), dict), "Learn .NET, not asp.net or cd .. ");
}

#[test]
fn spellings_with_quotes_and_phrases_match_only_as_written() {
    // t3code: exact aliases and phrase boundaries.
    let dict = r#"
        [[terms]]
        text = "AI"
        sounds_like = ["ay eye"]
        [[terms]]
        text = "Bob"
        sounds_like = ["Robert"]
        [[terms]]
        text = "Roberts"
        [[terms]]
        text = "O'Reilly"
        sounds_like = ['"the publisher"']
        [[terms]]
        text = "GitHub"
        sounds_like = ["git hub"]
    "#;
    let spoken = "a test with ay eye. Robert, Roberts. \"the publisher\" and the publisher. (git hub), git (hub). \
                  git hub's API, git hub’s API, git’s hub.";
    let expected = "A test with AI. Bob, Roberts. O'Reilly and the publisher. (GitHub), git (hub). \
                    GitHub's API, GitHub’s API, git’s hub. ";
    assert_eq!(text(&said(spoken), dict), expected);
}
