use psst_config::{ConfigFile, DictionaryFile};
use psst_dict::{Dictionary, Error};

fn dictionary(toml: &str) -> Dictionary {
    Dictionary::from_file(&DictionaryFile::from_toml_str(toml).unwrap()).unwrap()
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
}

#[test]
fn an_empty_file_gives_an_empty_dictionary() {
    let dict = Dictionary::from_file(&DictionaryFile::default()).unwrap();
    assert!(dict.terms().is_empty());
    assert!(dict.bias_terms("en").is_empty());
}

#[test]
fn two_terms_with_one_identity_are_rejected() {
    let file = DictionaryFile::from_toml_str("[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"gpu\"").unwrap();
    assert!(matches!(Dictionary::from_file(&file), Err(Error::DuplicateTerm { .. })));
}
