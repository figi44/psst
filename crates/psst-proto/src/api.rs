//! HTTP routes and their bodies (plan 5.2): Phase 1, the dictionary routes of Phase 2, and the
//! profile and login routes of Phase 3. All bodies are JSON, except the login redemption, which
//! is a browser form.
//!
//! Errors from `/api/*` come back with a non-2xx status and an [`ApiError`] body.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{ClientTimings, DeviceId, Final, HistoryId, ServerTimings, SessionId, TermId, Token};

/// Route paths. `{id}` is a path parameter.
pub mod routes {
    /// The client WebSocket ([`crate::ws`]).
    pub const CLIENT: &str = "/v1/client";
    /// `GET` → [`super::Health`].
    pub const HEALTH: &str = "/health";
    /// `POST` [`super::PairRedeem`] → [`super::PairRedeemed`].
    pub const PAIR_REDEEM: &str = "/api/pair/redeem";
    /// `GET` → [`super::ModelStatus`].
    pub const MODEL: &str = "/api/model";
    /// `POST` → [`super::ModelStatus`].
    pub const MODEL_LOAD: &str = "/api/model/load";
    /// `POST` → [`super::ModelStatus`].
    pub const MODEL_UNLOAD: &str = "/api/model/unload";
    /// `POST` [`super::PairingRequest`] → [`super::Pairing`].
    pub const PAIRING: &str = "/api/pairing";
    /// `GET` → `[`[`super::Device`]`]`.
    pub const DEVICES: &str = "/api/devices";
    /// `DELETE` → `204 No Content`.
    pub const DEVICE: &str = "/api/devices/{id}";
    /// `GET` → [`super::Stats`].
    pub const STATS: &str = "/api/stats";
    /// `GET ?limit=` ([`super::HistoryQuery`]) → `[`[`super::HistoryRow`]`]`, newest first.
    pub const HISTORY: &str = "/api/history";
    /// `GET` → [`super::DictionaryView`].
    pub const DICTIONARY: &str = "/api/dictionary";
    /// `POST` [`super::Term`] → [`super::TermChange`]: adds a term.
    pub const DICTIONARY_TERMS: &str = "/api/dictionary/terms";
    /// `PUT` [`super::Term`] → [`super::TermChange`]: replaces a term. `DELETE` →
    /// [`super::TermChange`]: hides a file term, or removes an added one. `{id}` is any spelling
    /// with the term's identity.
    pub const DICTIONARY_TERM: &str = "/api/dictionary/terms/{id}";
    /// `POST` → [`super::TermChange`]: deletes the database's entry for the identity.
    pub const DICTIONARY_TERM_RESET: &str = "/api/dictionary/terms/{id}/reset";
    /// `POST` [`super::NewNeverRule`] → [`super::NeverRule`].
    pub const DICTIONARY_NEVER: &str = "/api/dictionary/never";
    /// `DELETE` → `204 No Content`. `{id}` is a [`super::NeverRuleId`].
    pub const DICTIONARY_NEVER_RULE: &str = "/api/dictionary/never/{id}";
    /// `GET` → [`super::DictionaryExport`].
    pub const DICTIONARY_EXPORT: &str = "/api/dictionary/export";
    /// `POST` [`super::DictionaryTest`] → [`crate::Final`].
    pub const DICTIONARY_TEST: &str = "/api/dictionary/test";
    /// `GET` → [`super::ProfilesView`].
    pub const PROFILES: &str = "/api/profiles";
    /// `GET` → [`super::ProfilesExport`].
    pub const PROFILES_EXPORT: &str = "/api/profiles/export";
    /// `PUT` [`super::Profile`] → [`super::ProfileChange`]: stores the profile `{name}` over
    /// `profiles.toml`'s. Singular, so that every name (`export` too) has a path.
    pub const PROFILE: &str = "/api/profile/{name}";
    /// `POST` → [`super::ProfileChange`]: removes the database's version of the profile.
    pub const PROFILE_RESET: &str = "/api/profile/{name}/reset";
    /// `POST` → [`super::LoginLink`]. Over the socket or with a device token, not a browser
    /// session.
    pub const LOGIN_LINK: &str = "/api/login/link";
    /// `POST`, a browser form with `token`: sets the session cookie and redirects to the web UI.
    pub const LOGIN_REDEEM: &str = "/api/login/redeem";

    /// `template` with its `{id}` replaced by `id`, percent-encoded as one path segment.
    pub fn with_id(template: &str, id: &str) -> String {
        template.replace("{id}", &super::encode_segment(id))
    }

    /// `template` with its `{name}` replaced by `name`, percent-encoded as one path segment.
    pub fn with_name(template: &str, name: &str) -> String {
        template.replace("{name}", &super::encode_segment(name))
    }
}

/// Percent-encodes `segment` for use as one path segment: everything but unreserved characters.
pub fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Body of `GET /health`, with status 200 while the server is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
}

/// Error body of every `/api/*` route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    /// 400
    BadRequest,
    /// 401: no or unknown credentials.
    Unauthorized,
    /// 403: authenticated, but not allowed (for example a pairing code past its attempts).
    Forbidden,
    /// 404
    NotFound,
    /// 429
    RateLimited,
    /// 500
    Internal,
}

/// `POST /api/pair/redeem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRedeem {
    pub code: String,
    /// Name of the new device, for example `thinkpad`.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRedeemed {
    pub device_id: DeviceId,
    pub token: Token,
}

/// `POST /api/pairing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    pub name: String,
}

/// A one-time pairing code: 8 characters, valid 5 minutes, void after 5 wrong attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub code: String,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatus {
    /// The configured model, for example `large-v3-turbo`.
    pub name: String,
    pub state: ModelState,
    /// The resolved revision, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// While `downloading`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DownloadProgress>,
    /// Why the model is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `missing → downloading → loading → loaded`, plus `unloaded` (on purpose) and `failed`.
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
    /// A user on the server's machine, through the Unix socket.
    Local,
    /// A paired device with a token.
    Token,
}

/// `GET /api/stats`: p50/p95 per timing field over the retained history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Sessions the percentiles are computed over.
    pub sessions: u64,
    /// Keyed by `server.<field>` ([`ServerTimings`]), `client.<field>` ([`ClientTimings`]) and
    /// `network_estimate_ms` (the client's stop→final minus the server's; an estimate).
    pub timings: BTreeMap<String, Percentiles>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Percentiles {
    /// Sessions that have this field.
    pub count: u64,
    pub p50: u32,
    pub p95: u32,
}

/// Query of `GET /api/history`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One session's history row. Written at `start`, completed when the session finishes.
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
    /// Raw and final text with edits; absent until the session finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#final: Option<Final>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_timings: Option<ServerTimings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_timings: Option<ClientTimings>,
}

/// A dictionary term as the API carries it: the keys of a `[[terms]]` entry in
/// `dictionary.toml` (plan 7.1), each with the same default. Keys at their default are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    /// The exact spelling to produce. Its identity ([`TermId`]) is this after NFC and case folding.
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds_like: Vec<String>,
    /// `"any"`, or the one language code whose sessions use the term.
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
    /// A term with every other key at its default.
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

/// `GET /api/dictionary`: every identity that `dictionary.toml` or the database layer knows, and
/// the never rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryView {
    /// Sorted by identity.
    pub terms: Vec<DictionaryTerm>,
    /// Oldest first.
    pub never: Vec<NeverRule>,
    /// What `check-config` would warn about in the dictionary sessions use now.
    pub warnings: Vec<String>,
}

/// One identity, and where its version comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryTerm {
    pub id: TermId,
    pub layer: TermLayer,
    /// Whether sessions use [`DictionaryTerm::term`]. False for a hidden term, and for a database
    /// entry left out for [`DictionaryTerm::problem`].
    pub active: bool,
    /// The database's version for `added` and `replaced`, the file's for `file` and `hidden`.
    /// Absent for a hidden term that `dictionary.toml` no longer has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<Term>,
    /// The file's version, for `replaced`.
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
    /// From `dictionary.toml`, unchanged.
    File,
    /// Only in the database.
    Added,
    /// The database replaces the file's term with the same identity.
    Replaced,
    /// The database hides the file's term.
    Hidden,
}

/// The answer to a change of one term: the identity's entry afterwards, and what the change set
/// off in the plan 7.1 checks. A change that the checks reject gets `400 bad_request` instead,
/// and changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermChange {
    /// Absent when neither layer knows the identity any more (an added term removed or reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<DictionaryTerm>,
    /// Warnings the dictionary has after the change and didn't have before.
    pub warnings: Vec<String>,
}

/// Id of a never rule, assigned by the server.
pub type NeverRuleId = u64;

/// `POST /api/dictionary/never`: `term` (any spelling with its identity) must never replace
/// `span`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewNeverRule {
    pub term: String,
    pub span: String,
}

/// A never rule (plan 7.1): it vetoes `term` replacing a span whose fuzzy key is `span_key`, both
/// exactly and fuzzily.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeverRule {
    pub id: NeverRuleId,
    pub term: TermId,
    /// The span as it was given.
    pub span: String,
    /// What a span is compared by: lowercase, diacritics folded, letters and digits only.
    pub span_key: String,
    /// False while `term` is hidden or gone; the rule is kept.
    pub active: bool,
    pub created_at: Timestamp,
}

/// `GET /api/dictionary/export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryExport {
    /// The database layer in `dictionary.toml`'s format, for moving into the file (the host config's
    /// `settings.dictionary`): the added and replaced terms as `[[terms]]`, and hidden terms and
    /// never rules, which the file can't express, as comments.
    pub toml: String,
}

/// `POST /api/dictionary/test`: runs the pipeline on `text` as if the engine had produced it,
/// without probabilities, with the dictionary sessions use now and the `default` profile's rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictionaryTest {
    pub text: String,
    /// The session language; the `default` profile's when absent. It picks the terms, the common
    /// words and the fillers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// A profile as the API carries it: the keys of a `[profiles.<name>]` table in `profiles.toml`,
/// each with the same default. As in the file, unknown keys are errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Language to transcribe in (ISO 639-1).
    pub language: String,
    /// A sentence in the style you dictate, used as the start of the engine prompt as written.
    pub prompt_style: String,
    /// Post-processing rule ids (`fillers`, `whitespace`, `capitalize`, `trailing_space`),
    /// applied in this order.
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

/// `GET /api/profiles`: every profile sessions can use, and where its version comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesView {
    /// Sorted by name.
    pub profiles: Vec<ProfileEntry>,
    /// What `check-config` would warn about in the profiles sessions use now.
    pub warnings: Vec<String>,
}

/// One profile by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub name: String,
    pub layer: ProfileLayer,
    /// The version sessions use: the database's for `replaced` and `added`.
    pub profile: Profile,
    /// `profiles.toml`'s version, for `replaced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Profile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLayer {
    /// From `profiles.toml`, unchanged.
    File,
    /// The database's version replaces the file's.
    Replaced,
    /// Only in the database.
    Added,
}

/// The answer to a change of one profile. A profile that fails the `check-config` checks gets
/// `400 bad_request` instead, and changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileChange {
    /// The profile afterwards; absent when an added profile was reset, so it's gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileEntry>,
    /// What `check-config` warns about in this profile now, such as a long `prompt_style`.
    pub warnings: Vec<String>,
}

/// `GET /api/profiles/export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesExport {
    /// `profiles.toml` with the database's profiles in place of the file's, for moving into the
    /// file (the host config's `settings.profiles`).
    pub toml: String,
}

/// `POST /api/login/link`: a one-time URL that logs a browser into the web UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginLink {
    pub url: String,
    pub expires_at: Timestamp,
}
