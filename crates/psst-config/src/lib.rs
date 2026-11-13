//! The four config files and their single implementation (plan 5.5).
//!
//! | File | Type | Read by |
//! |---|---|---|
//! | `server.toml` | [`ServerConfig`] | server |
//! | `profiles.toml` | [`ProfilesFile`], resolved into [`Profile`]s | server, eval |
//! | `dictionary.toml` | [`DictionaryFile`] | server, eval |
//! | `client.toml` | [`ClientConfig`] | client |
//!
//! Every key has a default, so a missing file or key means its default. Unknown keys are errors.
//! `docs/config.md` documents every key with its default; a test keeps the two in step.

mod client;
mod dictionary;
mod profiles;
mod server;

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};

pub use client::{
    Anchor, AppRule, AudioConfig, AudioInput, ClientConfig, Color, InsertConfig, KNOWN_TERMINALS, OverlayConfig,
    PasteAction, PasteRule,
};
pub use dictionary::{DictionaryFile, TermLanguages, TermSpec};
pub use profiles::{DEFAULT_PROFILE, Profile, ProfileSpec, ProfilesFile, Rule};
pub use server::{DecodePath, Limits, ModelConfig, ModelOptions, ServerConfig, TermsAs};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{file}: {message}")]
    Parse { file: String, message: String },
    #[error("{file}: {message}")]
    Invalid { file: String, message: String },
}

/// Something allowed but probably unintended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub file: String,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: warning: {}", self.file, self.message)
    }
}

/// One of the four files.
pub trait ConfigFile: Default + Serialize + DeserializeOwned {
    /// The file's name, for example `server.toml`.
    const FILE_NAME: &'static str;

    /// Rules the types can't express. Returns the reason the file is invalid.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    /// Things that are allowed but probably unintended.
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }

    /// Parses and checks a file's contents.
    fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        parse(text, Self::FILE_NAME)
    }

    /// Reads, parses and checks a file.
    fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.into(), source })?;
        parse(&text, &path.display().to_string())
    }

    /// Like [`ConfigFile::load`], but a missing file gives the defaults.
    fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match fs::metadata(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            _ => Self::load(path),
        }
    }

    /// The file as TOML.
    fn to_toml_string(&self) -> String {
        toml::to_string(self).expect("config types always serialise")
    }
}

fn parse<T: ConfigFile>(text: &str, file: &str) -> Result<T, ConfigError> {
    let value: T =
        toml::from_str(text).map_err(|err| ConfigError::Parse { file: file.into(), message: err.to_string() })?;
    value.check().map_err(|message| ConfigError::Invalid { file: file.into(), message })?;
    Ok(value)
}

/// The three files the server reads from its config directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerFiles {
    pub server: ServerConfig,
    pub profiles: ProfilesFile,
    pub dictionary: DictionaryFile,
}

impl ServerFiles {
    /// Loads `server.toml`, `profiles.toml` and `dictionary.toml` from `dir`. A missing file
    /// gives its defaults.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        Ok(Self {
            server: ServerConfig::load_or_default(&dir.join(ServerConfig::FILE_NAME))?,
            profiles: ProfilesFile::load_or_default(&dir.join(ProfilesFile::FILE_NAME))?,
            dictionary: DictionaryFile::load_or_default(&dir.join(DictionaryFile::FILE_NAME))?,
        })
    }

    /// Writes the three files into `dir`.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        fs::write(dir.join(ServerConfig::FILE_NAME), self.server.to_toml_string())?;
        fs::write(dir.join(ProfilesFile::FILE_NAME), self.profiles.to_toml_string())?;
        fs::write(dir.join(DictionaryFile::FILE_NAME), self.dictionary.to_toml_string())
    }

    pub fn warnings(&self) -> Vec<Warning> {
        fn tag<T: ConfigFile>(file: &T) -> impl Iterator<Item = Warning> {
            file.warnings().into_iter().map(|message| Warning { file: T::FILE_NAME.into(), message })
        }
        tag(&self.server).chain(tag(&self.profiles)).chain(tag(&self.dictionary)).collect()
    }
}
