//! Never rules as data on the dictionary, and the match keys they compare.

use psst_config::{ConfigFile, DictionaryFile};
use psst_dict::{Dictionary, NeverRule, fuzzy_key};

fn dictionary(toml: &str) -> Dictionary {
    Dictionary::from_file(&DictionaryFile::from_toml_str(toml).unwrap()).unwrap()
}

#[test]
fn match_keys_are_lowercase_letters_and_digits_without_diacritics() {
    assert_eq!(fuzzy_key("Hyper-Land"), "hyperland");
    assert_eq!(fuzzy_key("cube. Nettie's"), "cubenetties");
    assert_eq!(fuzzy_key("Dvořák"), "dvorak");
    assert_eq!(fuzzy_key("Dvor\u{30c}ák"), "dvorak");
    assert_eq!(fuzzy_key("T3 code"), "t3code");
    assert_eq!(fuzzy_key("C++"), "c");
}

#[test]
fn a_rule_names_the_term_by_identity_and_the_span_by_match_key() {
    assert_eq!(
        NeverRule::new("Kubernetes", "Cube-Nettie"),
        NeverRule { term: "kubernetes".into(), span_key: "cubenettie".into() }
    );
}

#[test]
fn only_rules_for_terms_in_the_dictionary_are_active() {
    let dict = dictionary("[[terms]]\ntext = \"Kubernetes\"\n[[terms]]\ntext = \"Hyprland\"").with_never_rules([
        NeverRule::new("Kubernetes", "cube nettie"),
        NeverRule::new("Hidden", "hid then"),
        NeverRule::new("Hyprland", "hyper land"),
        NeverRule::new("KUBERNETES", "Cube Nettie"),
    ]);
    assert_eq!(
        dict.never_rules(),
        [NeverRule::new("Hyprland", "hyper land"), NeverRule::new("Kubernetes", "cube nettie")]
    );
    assert!(dict.vetoes("kubernetes", "cube-nettie"));
    assert!(!dict.vetoes("kubernetes", "hyper land"));
    assert!(!dict.vetoes("hidden", "hid then"));
}

#[test]
fn never_rules_are_part_of_a_dictionarys_identity() {
    let toml = "[[terms]]\ntext = \"Kubernetes\"";
    assert_eq!(dictionary(toml), dictionary(toml));
    assert_ne!(dictionary(toml), dictionary(toml).with_never_rules([NeverRule::new("Kubernetes", "cube nettie")]));
    assert_eq!(
        dictionary(toml).with_never_rules([NeverRule::new("Kubernetes", "x")]).with_never_rules([]),
        dictionary(toml)
    );
}

#[test]
fn an_exact_match_a_rule_vetoes_makes_way_for_a_shorter_one() {
    let dict = dictionary("[[terms]]\ntext = \"Claude Code\"\n[[terms]]\ntext = \"Claude\"")
        .with_never_rules([NeverRule::new("Claude Code", "claude code")]);
    let matches = dict.exact_matches("ask claude code", "en", |_| true);
    let found: Vec<(&str, &str)> =
        matches.iter().map(|m| (&"ask claude code"[m.range.clone()], m.term.text.as_str())).collect();
    assert_eq!(found, [("claude", "Claude")]);
}
