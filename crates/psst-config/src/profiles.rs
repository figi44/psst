use std::collections::BTreeMap;

use psst_proto::api::Profile as ApiProfile;
use serde::{Deserialize, Serialize};

use crate::ConfigFile;

pub const DEFAULT_PROFILE: &str = "default";

/// Reloads on SIGHUP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilesFile {
    pub profiles: BTreeMap<String, ProfileSpec>,
    /// A `[fillers]` table replaces every built-in list, not only those of its languages.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSpec {
    /// ISO 639-1.
    pub language: String,
    /// A sentence in the style you dictate, used as the start of the prompt exactly as written.
    pub prompt_style: String,
    /// Applied in this order.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    Fillers,
    Whitespace,
    /// Leaves the first letter alone when the first token is protected.
    Capitalize,
    TrailingSpace,
}

impl Rule {
    pub const ALL: [Rule; 4] = [Rule::Fillers, Rule::Whitespace, Rule::Capitalize, Rule::TrailingSpace];

    pub fn id(self) -> &'static str {
        match self {
            Self::Fillers => "fillers",
            Self::Whitespace => "whitespace",
            Self::Capitalize => "capitalize",
            Self::TrailingSpace => "trailing_space",
        }
    }

    pub fn from_id(id: &str) -> Result<Rule, String> {
        Self::ALL.into_iter().find(|rule| rule.id() == id).ok_or_else(|| {
            let ids = Self::ALL.map(|rule| format!("`{}`", rule.id())).join(", ");
            format!("unknown rule `{id}`, expected one of {ids}")
        })
    }
}

/// The API's profile (`/api/profiles`) has the file's keys and defaults, but rules are plain ids
/// there, so an unknown one is an error here.
impl TryFrom<ApiProfile> for ProfileSpec {
    type Error = String;

    fn try_from(profile: ApiProfile) -> Result<Self, String> {
        Ok(Self {
            language: profile.language,
            prompt_style: profile.prompt_style,
            rules: profile.rules.iter().map(|id| Rule::from_id(id)).collect::<Result<_, _>>()?,
        })
    }
}

impl From<&ProfileSpec> for ApiProfile {
    fn from(spec: &ProfileSpec) -> Self {
        Self {
            language: spec.language.clone(),
            prompt_style: spec.prompt_style.clone(),
            rules: spec.rules.iter().map(|rule| rule.id().to_owned()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub language: String,
    pub prompt_style: String,
    pub rules: Vec<Rule>,
    pub fillers: Vec<String>,
}

impl ProfilesFile {
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

    pub fn resolve_all(&self) -> Vec<Profile> {
        self.profiles.keys().filter_map(|name| self.resolve(name)).collect()
    }
}

pub const PROMPT_STYLE_WARN_TOKENS: usize = 100;

pub fn estimated_tokens(text: &str) -> usize {
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
                return Err(format!("profiles.{name}.rules lists `{}` twice", rule.id()));
            }
        }
        Ok(())
    }

    fn warnings(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|(_, spec)| estimated_tokens(&spec.prompt_style) > PROMPT_STYLE_WARN_TOKENS)
            .map(|(name, _)| {
                format!("profiles.{name}.prompt_style is over about 100 tokens; it crowds out dictionary terms")
            })
            .collect()
    }
}
