//! Loading, identity, the checks of plan 7.1, and bias order.

use psst_config::{ConfigFile, DictionaryFile, TermLanguages};
use psst_dict::{Dictionary, Error, term_id};

fn file(toml: &str) -> DictionaryFile {
    DictionaryFile::from_toml_str(toml).unwrap()
}

fn dictionary(toml: &str) -> Dictionary {
    Dictionary::from_file(&file(toml)).unwrap()
}

fn rejection(toml: &str) -> Error {
    Dictionary::from_file(&file(toml)).expect_err("the dictionary should be rejected")
}

fn warnings(toml: &str) -> Vec<String> {
    let warnings = dictionary(toml).warnings();
    assert!(warnings.iter().all(|w| w.file == "dictionary.toml"));
    warnings.into_iter().map(|w| w.message).collect()
}

#[test]
fn bias_terms_are_pinned_first_then_alphabetical_for_the_language() {
    let dict = dictionary(
        r#"
        [[terms]]
        text = "t3code"
        [[terms]]
        text = "Hyprland"
        [[terms]]
        text = "psst"
        pin = true
        [[terms]]
        text = "Dvořák"
        languages = "cs"
        [[terms]]
        text = "NixOS"
        languages = "en"
        pin = true
        "#,
    );
    assert_eq!(dict.bias_terms("en"), ["NixOS", "psst", "Hyprland", "t3code"]);
    assert_eq!(dict.bias_terms("cs"), ["psst", "Dvořák", "Hyprland", "t3code"]);
    assert_eq!(dict.bias_terms("de"), ["psst", "Hyprland", "t3code"]);
}

#[test]
fn bias_order_ignores_case() {
    let dict = dictionary("[[terms]]\ntext = \"zsh\"\n[[terms]]\ntext = \"Alacritty\"\n[[terms]]\ntext = \"bash\"");
    assert_eq!(dict.bias_terms("en"), ["Alacritty", "bash", "zsh"]);
}

#[test]
fn an_empty_file_gives_an_empty_dictionary() {
    let dict = Dictionary::from_file(&DictionaryFile::default()).unwrap();
    assert!(dict.terms().is_empty());
    assert!(dict.bias_terms("en").is_empty());
    assert!(dict.exact_matches("anything at all", "en", |_| true).is_empty());
    assert!(dict.warnings().is_empty());
}

#[test]
fn identity_is_the_text_after_nfc_and_case_folding() {
    assert_eq!(term_id("GPU"), "gpu");
    assert_eq!(term_id("Dvor\u{30c}ák"), term_id("Dvořák"));
    assert_eq!(term_id("STRASSE"), term_id("Straße"));
    assert_eq!(term_id("ΣΊΣΥΦΟΣ"), term_id("σίσυφος"));
}

#[test]
fn identity_keeps_punctuation_so_c_cpp_and_csharp_are_three_terms() {
    let dict = dictionary("[[terms]]\ntext = \"C\"\n[[terms]]\ntext = \"C++\"\n[[terms]]\ntext = \"C#\"");
    let ids: Vec<&str> = dict.terms().iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["c", "c++", "c#"]);
}

#[test]
fn two_terms_with_one_identity_are_rejected() {
    for toml in [
        "[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"gpu\"",
        "[[terms]]\ntext = \"Dvořák\"\n[[terms]]\ntext = \"Dvor\u{30c}ák\"",
        // Identity is global: the database layer keys on it.
        "[[terms]]\ntext = \"Go\"\nlanguages = \"en\"\n[[terms]]\ntext = \"go\"\nlanguages = \"cs\"",
    ] {
        assert!(matches!(rejection(toml), Error::DuplicateTerm { .. }), "{toml}");
    }
}

#[test]
fn a_spelling_two_terms_claim_in_a_shared_language_is_rejected() {
    let cases = [
        // One term's alias is another term's text, ignoring case.
        (
            "[[terms]]\ntext = \"Hyprland\"\n[[terms]]\ntext = \"Hypr\"\nsounds_like = [\"HYPRLAND\"]",
            "terms `Hyprland` and `Hypr` both claim the spelling `HYPRLAND` in every language",
        ),
        // Two terms share an alias.
        (
            "[[terms]]\ntext = \"NixOS\"\nsounds_like = [\"nix os\"]\n[[terms]]\ntext = \"Nix\"\nsounds_like = [\"Nix OS\"]",
            "terms `NixOS` and `Nix` both claim the spelling `Nix OS` in every language",
        ),
        // A term for every language shares its spelling with one for `en`.
        (
            "[[terms]]\ntext = \"Kubernetes\"\nsounds_like = [\"cube nettie\"]\n[[terms]]\ntext = \"Cube Net\"\nlanguages = \"en\"\nsounds_like = [\"cube nettie\"]",
            "terms `Kubernetes` and `Cube Net` both claim the spelling `cube nettie` in `en` sessions",
        ),
    ];
    for (toml, message) in cases {
        let err = rejection(toml);
        assert!(matches!(err, Error::SpellingConflict { .. }), "{toml}");
        assert_eq!(err.to_string(), message);
    }
}

#[test]
fn a_spelling_may_belong_to_different_terms_in_different_languages() {
    let dict = dictionary(
        r#"
        [[terms]]
        text = "Dvořák"
        languages = "cs"
        sounds_like = ["dvorak"]
        [[terms]]
        text = "Dvorak"
        languages = "en"
        "#,
    );
    let replaced = |lang| dict.exact_matches("dvorak", lang, |_| true)[0].term.text.clone();
    assert_eq!(replaced("cs"), "Dvořák");
    assert_eq!(replaced("en"), "Dvorak");
}

#[test]
fn a_term_may_repeat_its_own_spelling() {
    let dict = dictionary("[[terms]]\ntext = \"GPU\"\nsounds_like = [\"gpu\", \"g p u\", \"G P U\"]");
    assert_eq!(dict.terms()[0].languages, TermLanguages::Any);
    assert_eq!(dict.exact_matches("the g p u", "en", |_| true).len(), 1);
}

#[test]
fn a_spelling_that_is_a_common_word_is_a_warning() {
    assert_eq!(
        warnings("[[terms]]\ntext = \"Go\"\n[[terms]]\ntext = \"Nix\"\nsounds_like = [\"next\", \"nicks\"]"),
        [
            "term `Go` is a common en word: in en sessions it never changes the case of `go`, and doesn't protect it",
            "term `Nix`: sounds_like `next` is a common en word: in en sessions every `next` becomes `Nix`",
        ]
    );
    assert_eq!(
        warnings("[[terms]]\ntext = \"Windows\"\n[[terms]]\ntext = \"Je\"\nlanguages = \"cs\""),
        [
            "term `Windows` is a common en and cs word: in en and cs sessions it never changes the case of `windows`, and doesn't protect it",
            "term `Je` is a common cs word: in cs sessions it never changes the case of `je`, and doesn't protect it",
        ]
    );
}

#[test]
fn a_common_word_of_another_language_is_no_warning() {
    // "go" is common in English, and "je" in Czech.
    assert!(
        warnings("[[terms]]\ntext = \"Go\"\nlanguages = \"cs\"\n[[terms]]\ntext = \"je\"\nlanguages = \"en\"")
            .is_empty()
    );
    assert!(warnings("[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]").is_empty());
}

#[test]
fn pinned_terms_over_about_150_prompt_tokens_are_a_warning() {
    let terms = |count: usize, languages: &str| -> String {
        (0..count)
            .map(|i| {
                format!("[[terms]]\ntext = \"Pinnedterm{languages}{i:03}\"\nlanguages = \"{languages}\"\npin = true\n")
            })
            .collect()
    };
    // Each term with its separator is about 4.5 tokens.
    assert!(warnings(&terms(33, "any")).is_empty());
    assert_eq!(
        warnings(&terms(34, "any")),
        [
            "pinned terms for every language come to about 153 prompt tokens, over about 150; they crowd out the profile's style and every other term"
        ]
    );
    assert_eq!(
        warnings(&format!("{}{}{}", terms(20, "any"), terms(20, "cs"), terms(10, "en"))),
        [
            "pinned terms for `cs` sessions come to about 175 prompt tokens, over about 150; they crowd out the profile's style and every other term"
        ]
    );
}

#[test]
fn unpinned_terms_count_toward_no_budget() {
    let toml: String = (0..200).map(|i| format!("[[terms]]\ntext = \"Unpinnedterm{i:03}\"\n")).collect();
    assert!(warnings(&toml).is_empty());
}
