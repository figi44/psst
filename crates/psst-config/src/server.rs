use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};

use psst_proto::DEFAULT_SOCKET;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ConfigFile;

/// Changes need a server restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: Vec<SocketAddr>,
    /// Where TCP connections may come from. psst has no TLS, so beyond this machine and its
    /// tailnet, anyone on the network could read tokens and dictations.
    pub allow_from: Vec<Network>,
    /// Ignored when systemd passes the socket in.
    pub socket: PathBuf,
    /// Socket users, as admins. When empty, `serve` without systemd accepts its own user.
    pub local_users: Vec<String>,
    /// Tailnet machines, by MagicDNS short name, that pair themselves over TCP. tailscaled vouches
    /// for which machine a connection comes from. Each gets one token; one dropped from the list
    /// is revoked at the next start.
    pub auto_pair: Vec<String>,
    /// tailscaled's LocalAPI socket, asked who an `auto_pair` caller is.
    pub tailscale_socket: PathBuf,
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
            allow_from: Network::LOCAL_AND_TAILNET.to_vec(),
            socket: PathBuf::from(DEFAULT_SOCKET),
            local_users: Vec::new(),
            auto_pair: Vec::new(),
            tailscale_socket: PathBuf::from("/run/tailscale/tailscaled.sock"),
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
        if let Some(name) = self.auto_pair.iter().find(|name| !is_short_name(name)) {
            return Err(format!(
                "auto_pair takes tailnet machines' MagicDNS short names, such as \"thinkpad\", not `{name}`"
            ));
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

/// An IP network, written `192.168.1.0/24`, or a single address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    addr: IpAddr,
    prefix: u8,
}

impl Network {
    /// This machine, and the addresses Tailscale gives a tailnet's machines.
    pub const LOCAL_AND_TAILNET: [Network; 4] = [
        Network { addr: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), prefix: 8 },
        Network { addr: IpAddr::V6(Ipv6Addr::LOCALHOST), prefix: 128 },
        Network { addr: IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), prefix: 10 },
        Network { addr: IpAddr::V6(Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0)), prefix: 48 },
    ];

    pub fn contains(&self, ip: IpAddr) -> bool {
        let differ = match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => u128::from(net.to_bits() ^ ip.to_bits()) << 96,
            (IpAddr::V6(net), IpAddr::V6(ip)) => net.to_bits() ^ ip.to_bits(),
            _ => return false,
        };
        differ.checked_shr(128 - u32::from(self.prefix)).unwrap_or(0) == 0
    }

    pub fn covers(&self, other: &Network) -> bool {
        self.prefix <= other.prefix && self.contains(other.addr)
    }
}

impl FromStr for Network {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let wrong = || format!("expected a network such as 192.168.1.0/24, not `{text}`");
        let (addr, prefix) = text.split_once('/').map_or((text, None), |(addr, prefix)| (addr, Some(prefix)));
        let addr: IpAddr = addr.parse().map_err(|_| wrong())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = match prefix {
            None => max,
            Some(prefix) => prefix.parse().ok().filter(|prefix| *prefix <= max).ok_or_else(wrong)?,
        };
        match addr.to_canonical() {
            // An IPv4-mapped network is the IPv4 network it maps, with its prefix counted in IPv4 bits.
            IpAddr::V4(v4) if addr.is_ipv6() => {
                Ok(Network { addr: IpAddr::V4(v4), prefix: prefix.checked_sub(96).ok_or_else(wrong)? })
            }
            addr => Ok(Network { addr, prefix }),
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Serialize for Network {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

/// A DNS label as MagicDNS writes it: lowercase, so it compares as is.
fn is_short_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn web_origin(url: &str) -> Option<&str> {
    let url = url.strip_suffix('/').unwrap_or(url);
    let authority = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let valid = !authority.is_empty()
        && authority.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_:[]".contains(&b))
        && !authority.starts_with(':');
    valid.then_some(url)
}
