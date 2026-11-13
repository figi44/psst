//! What `Dictionary::fuzzy_matches` reports (plan 7.2 steps 3–7). The rules themselves are tested
//! through the pipeline, in `psst-pipeline/tests/it/fuzzy.rs`.

use std::ops::Range;

use psst_config::{ConfigFile, DictionaryFile};
use psst_dict::{Dictionary, FuzzyToken};

fn dictionary(texts: &[&str]) -> Dictionary {
    let toml: String = texts.iter().map(|t| format!("[[terms]]\ntext = {t:?}\n")).collect();
    Dictionary::from_file(&DictionaryFile::from_toml_str(&toml).unwrap()).unwrap()
}

/// The text's whitespace-separated tokens, all available, all with probability `prob`.
fn tokens(text: &str, prob: Option<f32>) -> Vec<FuzzyToken> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                tokens.push(FuzzyToken { range: s..i, prob, available: true });
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    tokens
}

/// Each match as (replaced text, its tokens, term, score).
fn found<'t>(dict: &Dictionary, text: &'t str, tokens: &[FuzzyToken]) -> Vec<(&'t str, Range<usize>, String, f32)> {
    dict.fuzzy_matches(text, tokens, "en")
        .into_iter()
        .map(|m| (&text[m.range], m.tokens, m.term.text.clone(), m.score))
        .collect()
}

#[test]
fn a_match_replaces_the_span_without_its_punctuation_or_a_kept_suffix() {
    let dict = dictionary(&["Kubernetes", "Hyprland"]);
    let text = "on (cuber-netes's) pods, \"hyper land\".";
    let matches = found(&dict, text, &tokens(text, None));
    assert_eq!(
        matches.iter().map(|(span, tokens, term, _)| (*span, tokens.clone(), term.as_str())).collect::<Vec<_>>(),
        [("cuber-netes", 1..2, "Kubernetes"), ("hyper land", 3..5, "Hyprland"),]
    );
}

#[test]
fn the_score_is_the_edit_distance_over_the_longer_key_and_less_when_it_sounds_alike() {
    let dict = dictionary(&["Hyprland", "Docker", "Kafka", "CTranslate2", "Kubernetes"]);
    let unsure = |text: &str| found(&dict, text, &tokens(text, Some(0.3))).into_iter().map(|m| m.3).collect::<Vec<_>>();
    assert_eq!(unsure("hyprland"), [0.0]);
    // "docket" doesn't sound like Docker.
    assert_eq!(unsure("docket"), [1.0 / 6.0]);
    assert_eq!(unsure("kafla"), [1.0 / 5.0]);
    // Digits never sound alike.
    assert_eq!(unsure("translate3"), [2.0 / 11.0]);
    assert_eq!(unsure("hyper land"), [(1.0 / 9.0 * 0.3) as f32]);
    assert_eq!(unsure("kubernets"), [(1.0 / 10.0 * 0.3) as f32]);
}

#[test]
fn unavailable_tokens_other_languages_and_terms_without_fuzzy_take_no_part() {
    let dict = dictionary(&["Hyprland"]);
    let text = "hyper land";
    let mut held = tokens(text, None);
    held[1].available = false;
    assert!(found(&dict, text, &held).is_empty());
    assert!(dict.fuzzy_matches(text, &tokens(text, None), "cs").is_empty());
    let off =
        Dictionary::from_file(&DictionaryFile::from_toml_str("[[terms]]\ntext = \"Hyprland\"\nfuzzy = false").unwrap())
            .unwrap();
    assert!(off.fuzzy_matches(text, &tokens(text, None), "en").is_empty());
    assert_eq!(found(&dict, text, &tokens(text, None)).len(), 1);
}
