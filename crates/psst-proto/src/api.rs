use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{ClientTimings, DeviceId, Final, HistoryId, ServerTimings, SessionId, TermId, Token};

pub mod routes {
    pub const CLIENT: &str = "/v1/client";
    pub const HEALTH: &str = "/health";
    pub const PAIR_REDEEM: &str = "/api/pair/redeem";
    /// No body: the server asks tailscaled which machine is calling, and checks its `auto_pair`.
    pub const PAIR_AUTO: &str = "/api/pair/auto";
    pub const MODEL: &str = "/api/model";
    pub const MODEL_LOAD: &str = "/api/model/load";
    pub const MODEL_UNLOAD: &str = "/api/model/unload";
    pub const PAIRING: &str = "/api/pairing";
    pub const DEVICES: &str = "/api/devices";
    pub const DEVICE: &str = "/api/devices/{id}";
    pub const STATS: &str = "/api/stats";
    /// Newest first.
    pub const HISTORY: &str = "/api/history";
    pub const DICTIONARY: &str = "/api/dictionary";
    pub const DICTIONARY_TERMS: &str = "/api/dictionary/terms";
    /// `{id}` is any spelling with the term's identity. `DELETE` hides a file term, or removes an
    /// added one.
    pub const DICTIONARY_TERM: &str = "/api/dictionary/terms/{id}";
    pub const DICTIONARY_TERM_RESET: &str = "/api/dictionary/terms/{id}/reset";
    pub const DICTIONARY_NEVER: &str = "/api/dictionary/never";
    pub const DICTIONARY_NEVER_RULE: &str = "/api/dictionary/never/{id}";
    pub const DICTIONARY_EXPORT: &str = "/api/dictionary/export";
    pub const DICTIONARY_TEST: &str = "/api/dictionary/test";
    pub const PROFILES: &str = "/api/profiles";
    pub const PROFILES_EXPORT: &str = "/api/profiles/export";
    /// Singular, so that every name (`export` too) has a path.
    pub const PROFILE: &str = "/api/profile/{name}";
    pub const PROFILE_RESET: &str = "/api/profile/{name}/reset";
    /// Over the socket or with a device token, not a browser session.
    pub const LOGIN_LINK: &str = "/api/login/link";
    /// A browser form with `token`, not JSON.
    pub const LOGIN_REDEEM: &str = "/api/login/redeem";

    pub fn with_id(template: &str, id: &str) -> String {
        template.replace("{id}", &super::encode_segment(id))
    }

    pub fn with_name(template: &str, name: &str) -> String {
        template.replace("{name}", &super::encode_segment(name))
    }
}

pub fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
}

/// The body of every non-2xx answer from `/api/*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    RateLimited,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRedeem {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRedeemed {
    pub device_id: DeviceId,
    pub token: Token,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub code: String,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatus {
    pub name: String,
    pub state: ModelState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DownloadProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Missing,
    Downloading,
    Loading,
    Loaded,
    Unloaded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub kind: DeviceKind,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Local,
    Token,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub sessions: u64,
    /// Keyed by `server.<field>` ([`ServerTimings`]), `client.<field>` ([`ClientTimings`]) and
    /// `network_estimate_ms` (the client's stop→final minus the server's).
    pub timings: BTreeMap<String, Percentiles>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50: u32,
    pub p95: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only this device's rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
}

/// Written at `start`, completed when the session finishes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryRow {
    pub id: HistoryId,
    pub session_id: SessionId,
    pub device: DeviceId,
    pub profile: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#final: Option<Final>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_timings: Option<ServerTimings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_timings: Option<ClientTimings>,
}

/// The keys of a `[[terms]]` entry in `dictionary.toml`, with the same defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds_like: Vec<String>,
    /// `"any"`, or a single language code.
    #[serde(default = "any_language", skip_serializing_if = "is_any_language")]
    pub languages: String,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub fuzzy: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Term {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sounds_like: Vec::new(),
            languages: any_language(),
            fuzzy: true,
            pin: false,
            notes: None,
        }
    }
}

fn any_language() -> String {
    "any".into()
}

fn is_any_language(languages: &str) -> bool {
    languages == "any"
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryView {
    /// Sorted by identity.
    pub terms: Vec<DictionaryTerm>,
    /// Oldest first.
    pub never: Vec<NeverRule>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryTerm {
    pub id: TermId,
    pub layer: TermLayer,
    /// The version under the database's is psst's built-in one, which `dictionary.toml` doesn't
    /// have: [`TermLayer::File`] and [`DictionaryTerm::file`] then mean psst's version.
    #[serde(default, skip_serializing_if = "is_false")]
    pub built_in: bool,
    /// False for a hidden term, for a database entry left out for [`DictionaryTerm::problem`], and
    /// for a built-in term that gives way to another term claiming its spelling.
    pub active: bool,
    /// The database's version for `added` and `replaced`, the file's for `file` and `hidden`.
    /// Absent for a hidden term that `dictionary.toml` no longer has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<Term>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Term>,
    /// Why the database's entry is left out: it conflicts with `dictionary.toml` (which can happen
    /// when the file changes) or with an older entry. A replaced term then falls back to the
    /// file's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermLayer {
    File,
    Added,
    Replaced,
    Hidden,
}

/// A change that the conflict checks reject gets `400 bad_request` instead, and changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermChange {
    /// Absent when neither layer knows the identity any more (an added term removed or reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<DictionaryTerm>,
    /// Only the warnings the change added.
    pub warnings: Vec<String>,
}

pub type NeverRuleId = u64;

/// `term` is any spelling with the term's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewNeverRule {
    pub term: String,
    pub span: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeverRule {
    pub id: NeverRuleId,
    pub term: TermId,
    pub span: String,
    pub span_key: String,
    /// False while `term` is hidden or gone; the rule is kept.
    pub active: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryExport {
    /// Only the database layer, in `dictionary.toml`'s format. Hidden terms and never rules, which
    /// the file can't express, come as comments.
    pub toml: String,
}

/// Runs the pipeline on `text` as if the engine had produced it, without probabilities, with the
/// `default` profile's rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryTest {
    pub text: String,
    /// The `default` profile's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// The keys of a `[profiles.<name>]` table in `profiles.toml`, with the same defaults. As in the
/// file, unknown keys are errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// ISO 639-1.
    pub language: String,
    pub prompt_style: String,
    /// Applied in this order.
    pub rules: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            language: "en".into(),
            prompt_style: String::new(),
            rules: ["fillers", "whitespace", "capitalize", "trailing_space"].map(String::from).into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesView {
    /// Sorted by name.
    pub profiles: Vec<ProfileEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub name: String,
    pub layer: ProfileLayer,
    pub profile: Profile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Profile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLayer {
    File,
    Replaced,
    Added,
}

/// A profile that fails the `check-config` checks gets `400 bad_request` instead, and changes
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileChange {
    /// Absent when an added profile was reset, so it's gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileEntry>,
    /// Every warning the profile has now, not only new ones.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesExport {
    /// The whole `profiles.toml`, with the database's profiles in place of the file's.
    pub toml: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginLink {
    pub url: String,
    pub expires_at: Timestamp,
}
