use std::{net::SocketAddr, path::PathBuf};

use psst_proto::DEFAULT_SOCKET;
use serde::{Deserialize, Serialize};

use crate::ConfigFile;

/// Changes need a server restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: Vec<SocketAddr>,
    /// Ignored when systemd passes the socket in.
    pub socket: PathBuf,
    /// Socket users, as admins. When empty, `serve` without systemd accepts its own user.
    pub local_users: Vec<String>,
    /// Longest an accepted session waits for the model, reloads between attempts included.
    pub load_deadline_s: u32,
    pub history_days: u32,
    /// Accept `start.eval` and send `trace` with every `final`. For evals only.
    pub eval_mode: bool,
    /// The start of every login link. Unset, a link uses the address the client reached the
    /// server by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    pub limits: Limits,
    pub model: ModelConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: vec![
                SocketAddr::from(([0, 0, 0, 0], psst_proto::DEFAULT_PORT)),
                SocketAddr::from(([0u16; 8], psst_proto::DEFAULT_PORT)),
            ],
            socket: PathBuf::from(DEFAULT_SOCKET),
            local_users: Vec::new(),
            load_deadline_s: 120,
            history_days: 30,
            eval_mode: false,
            web_url: None,
            limits: Limits::default(),
            model: ModelConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Clients stop at it; audio more than 5 s past it gets `too_long`.
    pub max_recording_s: u32,
    pub max_active_sessions: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_recording_s: 600, max_active_sessions: 4 }
    }
}

/// Only the file can set these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub engine: String,
    pub command: Vec<String>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub device: String,
    pub compute_type: String,
    pub preload: bool,
    pub options: ModelOptions,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            engine: "faster-whisper".into(),
            command: vec!["psst-engine-faster-whisper".into()],
            model: "large-v3-turbo".into(),
            revision: None,
            device: "cuda".into(),
            compute_type: "int8_float16".into(),
            preload: true,
            options: ModelOptions::default(),
        }
    }
}

/// The defaults are what the engine benchmarks (`scripts/bench/`) chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOptions {
    pub path: DecodePath,
    pub beam_size: u32,
    pub word_timestamps: bool,
    pub vad_filter: bool,
    pub terms_as: TermsAs,
    pub batch_size: u32,
}

impl Default for ModelOptions {
    fn default() -> Self {
        Self {
            path: DecodePath::Batched,
            beam_size: 5,
            word_timestamps: true,
            vad_filter: true,
            terms_as: TermsAs::Prompt,
            batch_size: 8,
        }
    }
}

impl ModelOptions {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("options always serialise")
    }
}

/// Only the batched path: the engine benchmarks chose it for every length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodePath {
    /// faster-whisper's `BatchedInferencePipeline`.
    Batched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermsAs {
    Prompt,
    Hotwords,
}

impl ConfigFile for ServerConfig {
    const FILE_NAME: &'static str = "server.toml";

    fn check(&self) -> Result<(), String> {
        if self.local_users.iter().any(String::is_empty) {
            return Err("local_users must not contain empty names".into());
        }
        if self.load_deadline_s == 0 {
            return Err("load_deadline_s must be at least 1".into());
        }
        if let Some(url) = &self.web_url
            && web_origin(url).is_none()
        {
            return Err(format!("web_url must be http://host[:port] or https://host[:port], not `{url}`"));
        }
        if self.limits.max_recording_s == 0 || self.limits.max_active_sessions == 0 {
            return Err("limits must be at least 1".into());
        }
        let model = &self.model;
        if model.command.first().is_none_or(String::is_empty) {
            return Err("model.command must name the engine program".into());
        }
        if model.model.is_empty() {
            return Err("model.model must not be empty".into());
        }
        let options = &model.options;
        if options.beam_size == 0 || options.batch_size == 0 {
            return Err("model.options.beam_size and batch_size must be at least 1".into());
        }
        if !options.vad_filter {
            return Err("model.options.vad_filter must be true: the batched path requires it".into());
        }
        Ok(())
    }
}

pub fn web_origin(url: &str) -> Option<&str> {
    let url = url.strip_suffix('/').unwrap_or(url);
    let authority = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let valid = !authority.is_empty()
        && authority.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_:[]".contains(&b))
        && !authority.starts_with(':');
    valid.then_some(url)
}
