//! Exact spellings (plan 7.2 step 2): one leftmost-longest, case-insensitive matcher over every
//! term's `text` and `sounds_like`.

use std::{cmp::Reverse, collections::HashMap, ops::Range};

use aho_corasick::AhoCorasick;
use psst_config::TermLanguages;

use crate::{Error, Result, Term, common::is_common_word, fold::Folded, fold::match_key};

/// What the pipeline does with an exact match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Replace the matched characters with the term's `text`, record the edit, and protect the
    /// token.
    Replace,
    /// The text already is the term's `text`: protect the token, with nothing to record.
    Keep,
    /// A common word that would at most change case: leave it alone and unprotected, so a term `Go`
    /// doesn't change every "go", and "go home" is still capitalised.
    Ignore,
}

/// One exact match in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactMatch<'d> {
    /// Byte range of the matched characters. Surrounding punctuation is never part of it.
    pub range: Range<usize>,
    pub term: &'d Term,
    pub outcome: Outcome,
}

/// Whether `c` can sit right before or after an exact match: whitespace, `. , ; : ! ? ( ) [ ] { }
/// " '`, and the typographic quotes `‘ ’ “ ” „` and ellipsis `…`. `+`, `#`, `-` and `/` can't, so
/// the term `C` never matches inside "c++", "c#" or "c/c++".
fn is_boundary(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '"'
                | '\''
                | '\u{2018}'
                | '\u{2019}'
                | '\u{201c}'
                | '\u{201d}'
                | '\u{201e}'
                | '\u{2026}'
        )
}

fn at_boundaries(text: &str, range: &Range<usize>) -> bool {
    text[..range.start].chars().next_back().is_none_or(is_boundary)
        && text[range.end..].chars().next().is_none_or(is_boundary)
}

/// The languages two terms share, if any.
pub(crate) fn shared_languages(a: &TermLanguages, b: &TermLanguages) -> Option<TermLanguages> {
    match (a, b) {
        (TermLanguages::Any, TermLanguages::Any) => Some(TermLanguages::Any),
        (TermLanguages::Any, TermLanguages::Only(l)) | (TermLanguages::Only(l), TermLanguages::Any) => {
            Some(TermLanguages::Only(l.clone()))
        }
        (TermLanguages::Only(a), TermLanguages::Only(b)) => (a == b).then(|| TermLanguages::Only(a.clone())),
    }
}

#[derive(Debug, Clone, Copy)]
struct Claim {
    term: usize,
    /// The spelling is the term's `text`, not one of its `sounds_like`.
    is_text: bool,
}

/// The automaton over every spelling, built once per dictionary.
#[derive(Debug, Clone, Default)]
pub(crate) struct Matcher {
    automaton: Option<AhoCorasick>,
    /// The terms claiming each pattern; at most one per language.
    claims: Vec<Vec<Claim>>,
}

impl Matcher {
    /// Builds the matcher, rejecting a spelling that two terms claim in a shared language.
    pub fn new(terms: &[Term]) -> Result<Self> {
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut patterns: Vec<String> = Vec::new();
        let mut claims: Vec<Vec<Claim>> = Vec::new();
        for (i, term) in terms.iter().enumerate() {
            let spellings = std::iter::once((&term.text, true)).chain(term.sounds_like.iter().map(|s| (s, false)));
            let mut own: Vec<usize> = Vec::new();
            for (spelling, is_text) in spellings {
                let key = match_key(spelling.trim());
                if key.is_empty() {
                    continue;
                }
                let pattern = *index.entry(key).or_insert_with_key(|key| {
                    patterns.push(key.clone());
                    claims.push(Vec::new());
                    patterns.len() - 1
                });
                if own.contains(&pattern) {
                    continue;
                }
                own.push(pattern);
                for other in &claims[pattern] {
                    let first = &terms[other.term];
                    if let Some(languages) = shared_languages(&first.languages, &term.languages) {
                        return Err(Error::SpellingConflict {
                            spelling: spelling.trim().to_owned(),
                            first: first.text.clone(),
                            second: term.text.clone(),
                            languages,
                        });
                    }
                }
                claims[pattern].push(Claim { term: i, is_text });
            }
        }
        let automaton = match patterns.is_empty() {
            true => None,
            false => Some(AhoCorasick::new(&patterns).map_err(|err| Error::TooLarge(err.to_string()))?),
        };
        Ok(Self { automaton, claims })
    }

    /// Leftmost-longest exact matches of `language`'s terms in `text`, among the candidates
    /// `allow` accepts.
    pub fn find<'d>(
        &self,
        terms: &'d [Term],
        text: &str,
        language: &str,
        mut allow: impl FnMut(Range<usize>) -> bool,
    ) -> Vec<ExactMatch<'d>> {
        let Some(automaton) = &self.automaton else { return Vec::new() };
        let folded = Folded::new(text);
        let mut candidates: Vec<(Range<usize>, Claim)> = automaton
            .find_overlapping_iter(folded.text.as_str())
            .filter_map(|m| {
                let range = folded.original_range(m.start(), m.end())?;
                if !at_boundaries(text, &range) {
                    return None;
                }
                let claim =
                    self.claims[m.pattern().as_usize()].iter().find(|c| terms[c.term].languages.includes(language))?;
                Some((range, *claim))
            })
            .collect();
        candidates.sort_by_key(|(range, _)| (range.start, Reverse(range.end)));

        let mut matches = Vec::new();
        let mut taken = 0;
        for (range, claim) in candidates {
            if range.start < taken || !allow(range.clone()) {
                continue;
            }
            taken = range.end;
            let term = &terms[claim.term];
            let matched = &text[range.clone()];
            let outcome = if matched == term.text || (claim.is_text && is_common_word(language, matched)) {
                if is_common_word(language, matched) { Outcome::Ignore } else { Outcome::Keep }
            } else {
                Outcome::Replace
            };
            matches.push(ExactMatch { range, term, outcome });
        }
        matches
    }
}
