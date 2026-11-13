//! The dictionary (plan 7): loading, layering, bias terms and matching. Pure; no I/O.
//!
//! This is the contract stub: terms load from `dictionary.toml` and [`Dictionary::bias_terms`]
//! orders them. Identity keys with case folding, the conflict checks of 7.1, the database layer
//! and the matcher arrive with the dictionary work package.

use psst_config::{DictionaryFile, TermLanguages};
use psst_proto::TermId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("terms `{first}` and `{second}` have the same identity `{id}`")]
    DuplicateTerm { id: TermId, first: String, second: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dictionary {
    terms: Vec<Term>,
}

/// A term's identity key (plan 7.1): its `text` after NFC and case folding.
///
/// Stub: lowercases only.
pub fn term_id(text: &str) -> TermId {
    text.to_lowercase()
}

impl Dictionary {
    /// Builds the dictionary from the file. The database layer comes in Phase 2.
    pub fn from_file(file: &DictionaryFile) -> Result<Dictionary> {
        let mut terms: Vec<Term> = Vec::with_capacity(file.terms.len());
        for spec in &file.terms {
            let id = term_id(&spec.text);
            if let Some(first) = terms.iter().find(|t| t.id == id) {
                return Err(Error::DuplicateTerm { id, first: first.text.clone(), second: spec.text.clone() });
            }
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
        Ok(Dictionary { terms })
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
}
