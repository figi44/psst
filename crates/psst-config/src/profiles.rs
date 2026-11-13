use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ConfigFile;

/// The profile used when a session names none.
pub const DEFAULT_PROFILE: &str = "default";

/// `profiles.toml`. Reloads on SIGHUP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilesFile {
    /// Profiles by name. Must include `default`.
    pub profiles: BTreeMap<String, ProfileSpec>,
    /// Filler words per language, for the `fillers` rule. The built-in lists apply only when the
    /// file has no `[fillers]` table.
    pub fillers: BTreeMap<String, Vec<String>>,
}

impl Default for ProfilesFile {
    fn default() -> Self {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect();
        Self {
            profiles: [(DEFAULT_PROFILE.to_owned(), ProfileSpec::default())].into(),
            fillers: [("en".to_owned(), words(&["um", "uh", "erm"])), ("cs".to_owned(), words(&["ehm", "eee"]))].into(),
        }
    }
}

/// One profile as written in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSpec {
    /// Language to transcribe in (ISO 639-1).
    pub language: String,
    /// A sentence in the style you dictate, used as the start of the prompt exactly as written.
    pub prompt_style: String,
    /// Post-processing rules, applied in this order.
    pub rules: Vec<Rule>,
}

impl Default for ProfileSpec {
    fn default() -> Self {
        Self {
            language: "en".into(),
            prompt_style: String::new(),
            rules: vec![Rule::Fillers, Rule::Whitespace, Rule::Capitalize, Rule::TrailingSpace],
        }
    }
}

/// Built-in post-processing rules (plan 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Removes a standalone filler word with the one comma attached to it.
    Fillers,
    /// Collapses runs of spaces left by removals.
    Whitespace,
    /// Capitalises the first letter, unless the first token is protected.
    Capitalize,
    /// Appends a space to non-empty text.
    TrailingSpace,
}

/// A profile resolved for use: its filler list filled in from its language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub language: String,
    pub prompt_style: String,
    pub rules: Vec<Rule>,
    pub fillers: Vec<String>,
}

impl ProfilesFile {
    /// The named profile, resolved.
    pub fn resolve(&self, name: &str) -> Option<Profile> {
        let spec = self.profiles.get(name)?;
        Some(Profile {
            name: name.to_owned(),
            language: spec.language.clone(),
            prompt_style: spec.prompt_style.clone(),
            rules: spec.rules.clone(),
            fillers: self.fillers.get(&spec.language).cloned().unwrap_or_default(),
        })
    }

    /// Every profile, resolved, by name.
    pub fn resolve_all(&self) -> Vec<Profile> {
        self.profiles.keys().filter_map(|name| self.resolve(name)).collect()
    }
}

/// A rough token count for a prompt: UTF-8 bytes / 4, rounded up.
fn estimated_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

impl ConfigFile for ProfilesFile {
    const FILE_NAME: &'static str = "profiles.toml";

    fn check(&self) -> Result<(), String> {
        if !self.profiles.contains_key(DEFAULT_PROFILE) {
            return Err(format!("a `{DEFAULT_PROFILE}` profile is required"));
        }
        for (name, spec) in &self.profiles {
            if name.is_empty() {
                return Err("profile names must not be empty".into());
            }
            if spec.language.is_empty() {
                return Err(format!("profiles.{name}.language must not be empty"));
            }
            if let Some(rule) =
                spec.rules.iter().enumerate().find_map(|(i, r)| spec.rules[..i].contains(r).then_some(r))
            {
                return Err(format!("profiles.{name}.rules lists {rule:?} twice"));
            }
        }
        Ok(())
    }

    fn warnings(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|(_, spec)| estimated_tokens(&spec.prompt_style) > 100)
            .map(|(name, _)| {
                format!("profiles.{name}.prompt_style is over about 100 tokens; it crowds out dictionary terms")
            })
            .collect()
    }
}
