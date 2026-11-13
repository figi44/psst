mod built_in;
mod common;
mod exact;
mod fold;
mod fuzzy;
mod key;
pub mod layers;
mod never;

use std::{collections::HashMap, fmt, ops::Range};

use psst_config::{ConfigFile, DictionaryFile, TermLanguages, TermSpec, Warning};
use psst_proto::TermId;

pub use built_in::built_in_terms;
pub use common::{COMMON_WORD_LANGUAGES, common_words, is_common_word};
pub use exact::{ExactMatch, Outcome};
pub use fold::term_id;
pub use fuzzy::{FUZZY_LANGUAGE, FuzzyMatch, FuzzyToken};
pub use key::fuzzy_key;
pub use layers::{Layer, LayerEntry, LayerTerm, Resolved};
pub use never::NeverRule;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("terms `{first}` and `{second}` have the same identity `{id}`")]
    DuplicateTerm { id: TermId, first: String, second: String },
    #[error("terms `{first}` and `{second}` both claim the spelling `{spelling}` {}", InLanguages(.languages))]
    SpellingConflict { spelling: String, first: String, second: String, languages: TermLanguages },
    #[error("the dictionary is too large to match: {0}")]
    TooLarge(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

struct InLanguages<'a>(&'a TermLanguages);

impl fmt::Display for InLanguages<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TermLanguages::Any => f.write_str("in every language"),
            TermLanguages::Only(language) => write!(f, "in `{language}` sessions"),
        }
    }
}

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

#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    terms: Vec<Term>,
    exact: exact::Matcher,
    fuzzy: fuzzy::Index,
    /// Sorted: `vetoes` binary-searches it.
    never: Vec<NeverRule>,
}

impl PartialEq for Dictionary {
    fn eq(&self, other: &Self) -> bool {
        self.terms == other.terms && self.never == other.never
    }
}

impl Eq for Dictionary {}

/// Out of the engine's 223 prompt tokens.
const PINNED_TOKEN_BUDGET: usize = 150;

/// For the terms as the prompt writes them: `a, b, c.`
fn estimated_tokens(terms: &[&str]) -> usize {
    (terms.join(", ").len() + 1).div_ceil(4)
}

impl Dictionary {
    pub fn from_file(file: &DictionaryFile) -> Result<Dictionary> {
        Self::from_specs(&file.terms)
    }

    fn from_specs<'a>(specs: impl IntoIterator<Item = &'a TermSpec>) -> Result<Dictionary> {
        let specs = specs.into_iter();
        let mut terms: Vec<Term> = Vec::with_capacity(specs.size_hint().0);
        let mut ids: HashMap<TermId, usize> = HashMap::with_capacity(specs.size_hint().0);
        for spec in specs {
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
        let fuzzy = fuzzy::Index::new(&terms);
        Ok(Dictionary { terms, exact, fuzzy, never: Vec::new() })
    }

    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    /// Most important first: the engine drops terms from the end to fit.
    pub fn bias_terms(&self, lang: &str) -> Vec<String> {
        let mut terms: Vec<&Term> = self.terms.iter().filter(|t| t.languages.includes(lang)).collect();
        terms.sort_by(|a, b| b.pin.cmp(&a.pin).then_with(|| a.id.cmp(&b.id)).then_with(|| a.text.cmp(&b.text)));
        terms.into_iter().map(|t| t.text.clone()).collect()
    }

    pub fn exact_matches(
        &self,
        text: &str,
        lang: &str,
        mut allow: impl FnMut(Range<usize>) -> bool,
    ) -> Vec<ExactMatch<'_>> {
        self.exact
            .find(&self.terms, text, lang, |range, term| !self.vetoes(&term.id, &text[range.clone()]) && allow(range))
    }

    pub fn fuzzy_matches(&self, text: &str, tokens: &[FuzzyToken], lang: &str) -> Vec<FuzzyMatch<'_>> {
        if lang != FUZZY_LANGUAGE {
            return Vec::new();
        }
        self.fuzzy.find(&self.terms, text, tokens, |term, span| self.vetoes(&term.id, span))
    }

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
