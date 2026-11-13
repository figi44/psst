use std::{cmp::Reverse, collections::HashMap, ops::Range};

use aho_corasick::AhoCorasick;
use psst_config::TermLanguages;

use crate::{Error, Result, Term, common::is_common_word, fold::Folded, fold::match_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Replace,
    Keep,
    /// A common word that would at most change case stays unprotected, so a term `Go` doesn't
    /// change every "go", and "go home" is still capitalised.
    Ignore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactMatch<'d> {
    pub range: Range<usize>,
    pub term: &'d Term,
    pub outcome: Outcome,
}

/// Not `+`, `#`, `-` or `/`, so the term `C` never matches inside "c++", "c#" or "c/c++".
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
    is_text: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Matcher {
    automaton: Option<AhoCorasick>,
    /// At most one claim per language and pattern, which `find` relies on.
    claims: Vec<Vec<Claim>>,
}

impl Matcher {
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

    pub fn find<'d>(
        &self,
        terms: &'d [Term],
        text: &str,
        language: &str,
        mut allow: impl FnMut(Range<usize>, &Term) -> bool,
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
            let term = &terms[claim.term];
            if range.start < taken || !allow(range.clone(), term) {
                continue;
            }
            taken = range.end;
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
