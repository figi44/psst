//! Text handling for the metrics (plan 7.3): normalisation, words and exact term occurrences.

use std::ops::Range;

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// The whitespace-separated tokens of `text`, as byte ranges.
pub fn tokens(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                tokens.push(s..i);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push(s..text.len());
    }
    tokens
}

/// The normalised words of `text` (plan 7.3):
///
/// - NFC, then lowercase;
/// - apostrophes and hyphens are kept only inside words (`’` counts as an apostrophe);
/// - every other character that isn't a letter, digit or combining mark separates words, so
///   punctuation and symbols disappear;
/// - whitespace collapses, and the filler words of the language are removed.
pub fn normalise(text: &str, fillers: &[String]) -> Vec<String> {
    normalise_tokens(text, fillers).into_iter().map(|(_, word)| word).collect()
}

/// Like [`normalise`], with the index (into [`tokens`]) of the token each word came from.
pub fn normalise_tokens(text: &str, fillers: &[String]) -> Vec<(usize, String)> {
    let fillers: Vec<String> = fillers.iter().map(|f| fold(f)).collect();
    let mut words = Vec::new();
    for (index, range) in tokens(text).into_iter().enumerate() {
        for word in token_words(&text[range]) {
            if !fillers.contains(&word) {
                words.push((index, word));
            }
        }
    }
    words
}

fn fold(text: &str) -> String {
    text.to_lowercase().nfc().collect()
}

fn token_words(token: &str) -> Vec<String> {
    let chars: Vec<char> = fold(token)
        .chars()
        .map(|c| match c {
            '’' | 'ʼ' | '‘' => '\'',
            '‐' | '‑' => '-',
            c => c,
        })
        .collect();
    let is_word = |c: char| c.is_alphanumeric() || is_combining_mark(c);
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let inside = i > 0 && is_word(chars[i - 1]) && chars.get(i + 1).is_some_and(|&next| is_word(next));
        if is_word(c) || (matches!(c, '\'' | '-') && inside) {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// How often `term` occurs in `text` with its exact spelling: case-sensitive, before
/// normalisation (only NFC applies to both). An occurrence starts and ends at the text's edge,
/// whitespace or punctuation (`. , ; : ! ? ( ) [ ] { } " '`, typographic quotes and dashes), so
/// `C` doesn't occur in `C++` or `C#`, and `Kubernetes` occurs in `Kubernetes's`.
pub fn count_term(text: &str, term: &str) -> usize {
    let text: String = text.nfc().collect();
    let term: String = term.nfc().collect();
    if term.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut from = 0;
    while let Some(pos) = text[from..].find(&term) {
        let start = from + pos;
        let end = start + term.len();
        let before = text[..start].chars().next_back().is_none_or(is_boundary);
        let after = text[end..].chars().next().is_none_or(is_boundary);
        if before && after {
            count += 1;
            from = end;
        } else {
            from = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    count
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || ".,;:!?()[]{}\"'’‘“”„«»…—–".contains(c)
}
