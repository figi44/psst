//! The 5,000 most frequent word forms per language (plan 7.2), from Common Voice's CC0 sentences.
//! `data/derive-common-words.py` generates the lists, and `evals/SOURCES.md` records their source.

use std::{collections::HashMap, sync::LazyLock};

use crate::fold::fold;

/// Languages that have a common-word list.
pub const COMMON_WORD_LANGUAGES: [&str; 2] = ["en", "cs"];

struct List {
    ranked: Vec<&'static str>,
    /// Each word's rank, from 1 for the most frequent.
    rank: HashMap<&'static str, usize>,
}

impl List {
    fn parse(file: &'static str) -> Self {
        let ranked: Vec<&str> = file.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect();
        let rank = ranked.iter().enumerate().map(|(i, word)| (*word, i + 1)).collect();
        Self { ranked, rank }
    }
}

static EN: LazyLock<List> = LazyLock::new(|| List::parse(include_str!("../data/common-words/en.txt")));
static CS: LazyLock<List> = LazyLock::new(|| List::parse(include_str!("../data/common-words/cs.txt")));

fn list(language: &str) -> Option<&'static List> {
    match language {
        "en" => Some(&EN),
        "cs" => Some(&CS),
        _ => None,
    }
}

/// The language's most frequent word forms, most frequent first: case-folded, NFC, with `'` as the
/// only apostrophe. Empty for a language without a list.
pub fn common_words(language: &str) -> &'static [&'static str] {
    list(language).map_or(&[], |list| &list.ranked)
}

/// Whether `word`, ignoring case, is one of the language's most frequent word forms. Always false
/// for a language without a list, and for anything with whitespace in it.
pub fn is_common_word(language: &str, word: &str) -> bool {
    common_rank(language, word).is_some()
}

/// The rank of `word`, ignoring case, among the language's most frequent word forms: 1 for the
/// most frequent. `None` where [`is_common_word`] is false.
pub(crate) fn common_rank(language: &str, word: &str) -> Option<usize> {
    let list = list(language)?;
    if word.chars().any(char::is_whitespace) {
        return None;
    }
    let word = match word.is_ascii() {
        true => word.to_ascii_lowercase(),
        false => fold(word).replace(['\u{2019}', '\u{2018}', '\u{02bc}'], "'"),
    };
    list.rank.get(word.as_str()).copied()
}
