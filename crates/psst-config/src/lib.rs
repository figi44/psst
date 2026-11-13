//! A missing file or key means its default. Unknown keys are errors.

mod client;
mod dictionary;
pub mod layers;
mod profiles;
mod server;

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};

pub use client::{
    Anchor, AppRule, AudioCodec, AudioConfig, AudioInput, ClientConfig, Color, InsertConfig, KNOWN_TERMINALS,
    MenuConfig, OverlayConfig, PasteAction, PasteRule, PostProcessingContext, Sounds,
};
pub use dictionary::{DictionaryFile, TermLanguages, TermSpec};
pub use layers::{KeyPath, Layerable, Layered, Problem};
pub use profiles::{
    DEFAULT_PROFILE, PROMPT_STYLE_WARN_TOKENS, Profile, ProfileSpec, ProfilesFile, Rule, estimated_tokens,
};
pub use server::{DecodePath, Limits, ModelConfig, ModelOptions, ServerConfig, TermsAs, web_origin};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{file}: {message}")]
    Parse { file: String, message: String },
    #[error("{file}: {message}")]
    Invalid { file: String, message: String },
}

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

pub trait ConfigFile: Default + Serialize + DeserializeOwned {
    const FILE_NAME: &'static str;

    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }

    fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        parse(text, Self::FILE_NAME)
    }

    fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.into(), source })?;
        parse(&text, &path.display().to_string())
    }

    fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match fs::metadata(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            _ => Self::load(path),
        }
    }

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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerFiles {
    pub server: ServerConfig,
    pub profiles: ProfilesFile,
    pub dictionary: DictionaryFile,
}

impl ServerFiles {
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        Ok(Self {
            server: ServerConfig::load_or_default(&dir.join(ServerConfig::FILE_NAME))?,
            profiles: ProfilesFile::load_or_default(&dir.join(ProfilesFile::FILE_NAME))?,
            dictionary: DictionaryFile::load_or_default(&dir.join(DictionaryFile::FILE_NAME))?,
        })
    }

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
