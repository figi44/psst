//! The dictionary (plan 7): loading, checks, bias terms, the common-word lists, never rules and
//! exact matching. Pure; no I/O.
//!
//! - [`Dictionary::from_file`] builds the dictionary from `dictionary.toml` and rejects the
//!   conflicts of plan 7.1; [`Dictionary::warnings`] lists what is allowed but probably unintended.
//!   `psst-server check-config` reports both.
//! - [`NeverRule`]s veto a term replacing a span ([`Dictionary::vetoes`]), compared by
//!   [`fuzzy_key`]. [`Dictionary::with_never_rules`] sets them.
//! - [`Dictionary::bias_terms`] orders a language's terms for the engine (7.2 step 1).
//! - [`Dictionary::exact_matches`] finds exact spellings in a text (7.2 step 2).
//! - [`is_common_word`] and [`common_words`] answer from the 5,000 most frequent word forms of
//!   English and Czech.
//!
//! Fuzzy matching and the database layer arrive in Phase 2.

mod common;
mod exact;
mod fold;
mod key;
mod never;

use std::{collections::HashMap, fmt, ops::Range};

use psst_config::{ConfigFile, DictionaryFile, TermLanguages, Warning};
use psst_proto::TermId;

pub use common::{COMMON_WORD_LANGUAGES, common_words, is_common_word};
pub use exact::{ExactMatch, Outcome};
pub use fold::term_id;
pub use key::fuzzy_key;
pub use never::NeverRule;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("terms `{first}` and `{second}` have the same identity `{id}`")]
    DuplicateTerm { id: TermId, first: String, second: String },
    /// One spelling (`text` or `sounds_like`, ignoring case) claimed by two terms that share a
    /// language.
    #[error("terms `{first}` and `{second}` both claim the spelling `{spelling}` {}", InLanguages(.languages))]
    SpellingConflict { spelling: String, first: String, second: String, languages: TermLanguages },
    #[error("the dictionary is too large to match: {0}")]
    TooLarge(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// "in every language" or "in `en` sessions".
struct InLanguages<'a>(&'a TermLanguages);

impl fmt::Display for InLanguages<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TermLanguages::Any => f.write_str("in every language"),
            TermLanguages::Only(language) => write!(f, "in `{language}` sessions"),
        }
    }
}

/// A term ready for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub id: TermId,
    pub text: String,
    pub sounds_like: Vec<String>,
    pub languages: TermLanguages,
    pub fuzzy: bool,
    pub pin: bool,
    pub notes: Option<String>,
}

/// The resolved dictionary a session snapshots at `start`.
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    terms: Vec<Term>,
    exact: exact::Matcher,
    /// Active never rules, sorted.
    never: Vec<NeverRule>,
}

impl PartialEq for Dictionary {
    fn eq(&self, other: &Self) -> bool {
        self.terms == other.terms && self.never == other.never
    }
}

impl Eq for Dictionary {}

/// Pinned terms alone shouldn't take more than about this many of the engine's 223 prompt tokens.
const PINNED_TOKEN_BUDGET: usize = 150;

/// A rough token count for terms in the engine's prompt (`a, b, c.`): UTF-8 bytes / 4, rounded up.
fn estimated_tokens(terms: &[&str]) -> usize {
    (terms.join(", ").len() + 1).div_ceil(4)
}

impl Dictionary {
    /// Builds the dictionary from the file. The database layer comes in Phase 2.
    ///
    /// Rejects two terms with one identity, and a spelling two terms both claim in a language
    /// they share, including one term's `sounds_like` matching another term's `text`.
    pub fn from_file(file: &DictionaryFile) -> Result<Dictionary> {
        let mut terms: Vec<Term> = Vec::with_capacity(file.terms.len());
        let mut ids: HashMap<TermId, usize> = HashMap::with_capacity(file.terms.len());
        for spec in &file.terms {
            let id = term_id(&spec.text);
            if let Some(&first) = ids.get(&id) {
                return Err(Error::DuplicateTerm { id, first: terms[first].text.clone(), second: spec.text.clone() });
            }
            ids.insert(id.clone(), terms.len());
            terms.push(Term {
                id,
                text: spec.text.clone(),
                sounds_like: spec.sounds_like.clone(),
                languages: spec.languages.clone(),
                fuzzy: spec.fuzzy,
                pin: spec.pin,
                notes: spec.notes.clone(),
            });
        }
        let exact = exact::Matcher::new(&terms)?;
        Ok(Dictionary { terms, exact, never: Vec::new() })
    }

    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    /// Terms to bias the engine with for a session in `lang` (plan 7.2 step 1): pinned terms,
    /// then the rest, each group alphabetical. The engine drops terms from the end to fit.
    pub fn bias_terms(&self, lang: &str) -> Vec<String> {
        let mut terms: Vec<&Term> = self.terms.iter().filter(|t| t.languages.includes(lang)).collect();
        terms.sort_by(|a, b| b.pin.cmp(&a.pin).then_with(|| a.id.cmp(&b.id)).then_with(|| a.text.cmp(&b.text)));
        terms.into_iter().map(|t| t.text.clone()).collect()
    }

    /// Exact spellings of `lang`'s terms in `text` (plan 7.2 step 2), in text order and never
    /// overlapping.
    ///
    /// Every term's `text` and `sounds_like` take part, compared ignoring case. A match starts and
    /// ends at whitespace, the start or end of the text, or one of `. , ; : ! ? ( ) [ ] { } " '`
    /// (and the typographic quotes `‘ ’ “ ” „` and `…`). Where matches overlap, the leftmost wins,
    /// then the longest, so "c++" is the term `C++` and never `C`. Never rules and `allow` veto a
    /// candidate before that choice, so a shorter or later match can take its place.
    pub fn exact_matches(
        &self,
        text: &str,
        lang: &str,
        mut allow: impl FnMut(Range<usize>) -> bool,
    ) -> Vec<ExactMatch<'_>> {
        self.exact.find(&self.terms, text, lang, |range, term| {
            !self.vetoes(&term.id, &text[range.clone()]) && allow(range)
        })
    }

    /// Things the file allows but probably doesn't intend (plan 7.1): a spelling that's a common
    /// word in a language the term applies to, and pinned terms that alone take more than about
    /// 150 prompt tokens.
    pub fn warnings(&self) -> Vec<Warning> {
        let mut messages = Vec::new();
        for term in &self.terms {
            let spellings = std::iter::once(&term.text).chain(&term.sounds_like);
            for (i, spelling) in spellings.enumerate() {
                let spelling = spelling.trim();
                let common: Vec<&str> = COMMON_WORD_LANGUAGES
                    .into_iter()
                    .filter(|lang| term.languages.includes(lang) && is_common_word(lang, spelling))
                    .collect();
                if common.is_empty() {
                    continue;
                }
                let langs = common.join(" and ");
                messages.push(if i == 0 {
                    format!(
                        "term `{spelling}` is a common {langs} word: in {langs} sessions it never changes the case of `{}`, and doesn't protect it",
                        spelling.to_lowercase()
                    )
                } else {
                    format!(
                        "term `{}`: sounds_like `{spelling}` is a common {langs} word: in {langs} sessions every `{spelling}` becomes `{}`",
                        term.text, term.text
                    )
                });
            }
        }

        let pinned = |lang: Option<&str>| -> Vec<&str> {
            self.terms
                .iter()
                .filter(|t| t.pin && lang.map_or(t.languages == TermLanguages::Any, |l| t.languages.includes(l)))
                .map(|t| t.text.as_str())
                .collect()
        };
        let everywhere = estimated_tokens(&pinned(None));
        if everywhere > PINNED_TOKEN_BUDGET {
            messages.push(format!(
                "pinned terms for every language come to about {everywhere} prompt tokens, over about {PINNED_TOKEN_BUDGET}; they crowd out the profile's style and every other term"
            ));
        } else {
            let mut languages: Vec<&str> = self
                .terms
                .iter()
                .filter_map(|t| match &t.languages {
                    TermLanguages::Only(lang) if t.pin => Some(lang.as_str()),
                    _ => None,
                })
                .collect();
            languages.sort_unstable();
            languages.dedup();
            for lang in languages {
                let tokens = estimated_tokens(&pinned(Some(lang)));
                if tokens > PINNED_TOKEN_BUDGET {
                    messages.push(format!(
                        "pinned terms for `{lang}` sessions come to about {tokens} prompt tokens, over about {PINNED_TOKEN_BUDGET}; they crowd out the profile's style and every other term"
                    ));
                }
            }
        }

        messages.into_iter().map(|message| Warning { file: DictionaryFile::FILE_NAME.into(), message }).collect()
    }
}
