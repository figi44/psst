use std::ops::Range;

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

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

pub fn normalise(text: &str, fillers: &[String]) -> Vec<String> {
    normalise_tokens(text, fillers).into_iter().map(|(_, word)| word).collect()
}

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

/// Case-sensitive and before normalisation, since term recall scores the exact spelling.
/// `'` is a boundary but `+` and `#` aren't, so `Kubernetes` occurs in `Kubernetes's` and `C`
/// doesn't occur in `C++` or `C#`.
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
