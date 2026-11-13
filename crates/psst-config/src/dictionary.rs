use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ConfigFile;

/// `dictionary.toml`. Reloads on SIGHUP. Conflicts between terms are checked by `psst-dict`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DictionaryFile {
    pub terms: Vec<TermSpec>,
}

/// One `[[terms]]` entry (plan 7.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermSpec {
    /// The exact spelling to produce.
    pub text: String,
    /// Other spellings the engine produces for it.
    #[serde(default)]
    pub sounds_like: Vec<String>,
    #[serde(default)]
    pub languages: TermLanguages,
    /// Fuzzy matching, for English sessions.
    #[serde(default = "yes")]
    pub fuzzy: bool,
    /// Always offered to the engine first.
    #[serde(default)]
    pub pin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

impl TermSpec {
    /// A term with every other key at its default.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sounds_like: Vec::new(),
            languages: TermLanguages::Any,
            fuzzy: true,
            pin: false,
            notes: None,
        }
    }
}

/// The session languages a term applies to: `"any"`, or one language code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TermLanguages {
    #[default]
    Any,
    Only(String),
}

impl TermLanguages {
    pub fn includes(&self, language: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Only(only) => only == language,
        }
    }
}

impl From<String> for TermLanguages {
    fn from(value: String) -> Self {
        if value == "any" { Self::Any } else { Self::Only(value) }
    }
}

impl From<TermLanguages> for String {
    fn from(value: TermLanguages) -> Self {
        value.to_string()
    }
}

impl fmt::Display for TermLanguages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("any"),
            Self::Only(language) => f.write_str(language),
        }
    }
}

impl ConfigFile for DictionaryFile {
    const FILE_NAME: &'static str = "dictionary.toml";

    fn check(&self) -> Result<(), String> {
        for term in &self.terms {
            if term.text.trim().is_empty() {
                return Err("terms.text must not be empty".into());
            }
            if term.sounds_like.iter().any(|s| s.trim().is_empty()) {
                return Err(format!("term `{}`: sounds_like entries must not be empty", term.text));
            }
            if term.languages == TermLanguages::Only(String::new()) {
                return Err(format!("term `{}`: languages must be \"any\" or a language code", term.text));
            }
        }
        Ok(())
    }
}
