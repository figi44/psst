//! The database layer on top of `dictionary.toml`: add, replace, hide, reset
//! and rename, conflicts, and never rules.

use psst_config::{ConfigFile, DictionaryFile, TermLanguages, TermSpec};
use psst_dict::{Dictionary, Error, Layer, LayerEntry, LayerTerm, NeverRule, Resolved, fuzzy_key, term_id};

fn file(toml: &str) -> DictionaryFile {
    DictionaryFile::from_toml_str(toml).unwrap()
}

fn term(text: &str, sounds_like: &[&str]) -> TermSpec {
    TermSpec { sounds_like: sounds_like.iter().map(|s| s.to_string()).collect(), ..TermSpec::new(text) }
}

/// A database entry that adds or replaces `spec`, saved at `saved`.
fn set(spec: TermSpec, saved: u64) -> (String, LayerTerm) {
    (term_id(&spec.text), LayerTerm { entry: LayerEntry::Set(spec), saved })
}

fn hide(text: &str) -> (String, LayerTerm) {
    (term_id(text), LayerTerm { entry: LayerEntry::Hide, saved: 0 })
}

fn layer(entries: impl IntoIterator<Item = (String, LayerTerm)>) -> Layer {
    Layer { terms: entries.into_iter().collect(), never: Vec::new() }
}

fn resolve(file: &DictionaryFile, layer: &Layer) -> Resolved {
    Dictionary::resolve(file, layer).expect("the file is valid")
}

/// The texts of the dictionary's terms, in its order.
fn texts(dictionary: &Dictionary) -> Vec<&str> {
    dictionary.terms().iter().map(|t| t.text.as_str()).collect()
}

/// What an exact match makes of `spoken` in an English session, if anything.
fn replaced(dictionary: &Dictionary, spoken: &str) -> Option<String> {
    dictionary.exact_matches(spoken, "en", |_| true).first().map(|m| m.term.text.clone())
}

const FILE: &str = r#"
[[terms]]
text = "psst"
[[terms]]
text = "Hyprland"
sounds_like = ["hyper land"]
[[terms]]
text = "Kubernetes"
sounds_like = ["cube nettie"]
"#;

#[test]
fn an_empty_layer_gives_the_file_alone() {
    let file = file(FILE);
    let resolved = resolve(&file, &Layer::default());
    assert_eq!(resolved.dictionary, Dictionary::from_file(&file).unwrap());
    assert!(resolved.skipped.is_empty());
}

#[test]
fn terms_only_the_database_has_follow_the_files_by_identity() {
    let layer = layer([set(term("t3code", &["t3 code"]), 1), set(term("Alacritty", &[]), 2)]);
    let resolved = resolve(&file(FILE), &layer);
    assert_eq!(texts(&resolved.dictionary), ["psst", "Hyprland", "Kubernetes", "Alacritty", "t3code"]);
    assert_eq!(replaced(&resolved.dictionary, "t3 code"), Some("t3code".into()));
    assert!(resolved.skipped.is_empty());
}

#[test]
fn a_database_term_replaces_the_file_term_with_its_identity_in_place() {
    let layer = layer([set(term("HyprLand", &["hyperland"]), 1)]);
    let dictionary = resolve(&file(FILE), &layer).dictionary;
    assert_eq!(texts(&dictionary), ["psst", "HyprLand", "Kubernetes"]);
    assert_eq!(replaced(&dictionary, "hyperland"), Some("HyprLand".into()));
    assert_eq!(replaced(&dictionary, "hyper land"), None, "the file's spellings go with its version");
}

#[test]
fn a_hidden_file_term_is_left_out_and_hiding_a_term_the_file_lacks_changes_nothing() {
    let file = file(FILE);
    let dictionary = resolve(&file, &layer([hide("HYPRLAND")])).dictionary;
    assert_eq!(texts(&dictionary), ["psst", "Kubernetes"]);
    assert_eq!(replaced(&dictionary, "hyper land"), None);
    assert_eq!(resolve(&file, &layer([hide("Wayland")])).dictionary, Dictionary::from_file(&file).unwrap());
}

#[test]
fn resetting_an_identity_brings_the_file_version_back() {
    let file = file(FILE);
    let mut layer = layer([hide("Kubernetes"), set(term("Hyprland", &["hyperland"]), 1), set(term("t3code", &[]), 2)]);
    for id in ["kubernetes", "hyprland", "t3code"] {
        layer.terms.remove(id);
    }
    assert_eq!(resolve(&file, &layer).dictionary, Dictionary::from_file(&file).unwrap());
}

#[test]
fn renaming_is_a_hide_plus_an_add_and_never_rules_for_the_old_identity_go_inactive() {
    let file = file(FILE);
    let never = vec![NeverRule::new("Kubernetes", "cube nettie")];
    let before = resolve(&file, &Layer { never: never.clone(), ..Layer::default() }).dictionary;
    assert_eq!(before.never_rules(), never);

    let renamed = Layer { never: never.clone(), ..layer([hide("Kubernetes"), set(term("K8s", &["cube nettie"]), 1)]) };
    let after = resolve(&file, &renamed).dictionary;
    assert_eq!(texts(&after), ["psst", "Hyprland", "K8s"]);
    assert_eq!(replaced(&after, "cube nettie"), Some("K8s".into()));
    assert!(after.never_rules().is_empty(), "the rule is kept in the layer, but inactive");
    assert!(!after.vetoes("kubernetes", "cube nettie"));

    let undone = Layer { never, ..Layer::default() };
    assert_eq!(resolve(&file, &undone).dictionary, before, "resetting both entries brings the rule back");
}

#[test]
fn a_database_term_that_conflicts_with_the_file_is_left_out() {
    // The file gained `Kubernetes` with this alias after the database added `Cube Net`.
    let layer = layer([set(term("Cube Net", &["Cube Nettie"]), 1), set(term("t3code", &[]), 2)]);
    let resolved = resolve(&file(FILE), &layer);
    assert_eq!(texts(&resolved.dictionary), ["psst", "Hyprland", "Kubernetes", "t3code"]);
    assert_eq!(
        resolved.skipped.into_iter().collect::<Vec<_>>(),
        [(
            "cube net".to_owned(),
            "terms `Kubernetes` and `Cube Net` both claim the spelling `Cube Nettie` in every language".to_owned()
        )]
    );
}

#[test]
fn a_replacement_that_conflicts_falls_back_to_the_file_version() {
    let layer = layer([set(term("Hyprland", &["cube nettie"]), 1)]);
    let resolved = resolve(&file(FILE), &layer);
    assert_eq!(resolved.dictionary, Dictionary::from_file(&file(FILE)).unwrap());
    assert_eq!(resolved.skipped.keys().collect::<Vec<_>>(), ["hyprland"]);
    assert_eq!(replaced(&resolved.dictionary, "hyper land"), Some("Hyprland".into()));
}

#[test]
fn of_two_conflicting_database_entries_the_one_saved_later_is_left_out() {
    let empty = DictionaryFile::default();
    for (zsh_saved, bash_saved, kept, skipped) in [(1, 2, "Zsh", "bash"), (2, 1, "Bash", "zsh")] {
        let layer = layer([set(term("Zsh", &["shell"]), zsh_saved), set(term("Bash", &["shell"]), bash_saved)]);
        let resolved = resolve(&empty, &layer);
        assert_eq!(texts(&resolved.dictionary), [kept]);
        assert_eq!(resolved.skipped.keys().collect::<Vec<_>>(), [skipped]);
    }
}

#[test]
fn spellings_conflict_only_in_a_shared_language() {
    let czech = TermSpec { languages: TermLanguages::Only("cs".into()), ..term("Kubernetes CZ", &["cube nettie"]) };
    let english = TermSpec { languages: TermLanguages::Only("en".into()), ..term("Kubernetes", &["cube nettie"]) };
    let file = DictionaryFile { terms: vec![english] };
    let resolved = resolve(&file, &layer([set(czech, 1)]));
    assert!(resolved.skipped.is_empty());
    assert_eq!(texts(&resolved.dictionary), ["Kubernetes", "Kubernetes CZ"]);
}

#[test]
fn an_entry_whose_term_has_another_identity_is_left_out() {
    let entry = ("hyprland".to_owned(), LayerTerm { entry: LayerEntry::Set(term("Wayland", &[])), saved: 1 });
    let resolved = resolve(&file(FILE), &layer([entry]));
    assert_eq!(texts(&resolved.dictionary), ["psst", "Hyprland", "Kubernetes"]);
    assert!(resolved.skipped["hyprland"].contains("another identity"), "{:?}", resolved.skipped);
}

#[test]
fn a_file_that_is_invalid_on_its_own_doesnt_resolve() {
    let file = file("[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"gpu\"");
    // Hiding one of the two doesn't make the file valid: check-config rejects it as it is.
    let err = Dictionary::resolve(&file, &layer([hide("gpu")])).expect_err("the file is invalid");
    assert!(matches!(err, Error::DuplicateTerm { .. }), "{err}");
}

#[test]
fn never_rules_veto_a_term_for_a_span_by_its_fuzzy_key() {
    let never = vec![NeverRule::new("KUBERNETES", "Cube Nettie"), NeverRule::new("Wayland", "way land")];
    let dictionary = resolve(&file(FILE), &Layer { never, ..Layer::default() }).dictionary;
    assert_eq!(dictionary.never_rules(), [NeverRule { term: "kubernetes".into(), span_key: "cubenettie".into() }]);
    for span in ["cube nettie", "Cube-Nettie", "cubenettie"] {
        assert!(dictionary.vetoes("kubernetes", span), "{span}");
    }
    assert!(!dictionary.vetoes("kubernetes", "cube net"));
    assert!(!dictionary.vetoes("hyprland", "cube nettie"));
    assert!(dictionary.clone().with_never_rules([]).never_rules().is_empty(), "with_never_rules replaces them");
}

#[test]
fn the_fuzzy_key_folds_case_and_diacritics_and_keeps_letters_and_digits() {
    assert_eq!(fuzzy_key("Cube Nettie"), "cubenettie");
    assert_eq!(fuzzy_key("Dvořák"), "dvorak");
    assert_eq!(fuzzy_key("t3-code's"), "t3codes");
    assert_eq!(fuzzy_key("C++"), "c");
    assert_eq!(fuzzy_key("ﬁle"), "file");
    assert_eq!(fuzzy_key("..."), "");
}
