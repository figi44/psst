use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{ClientTimings, DeviceId, Endpoint, Final, HistoryId, ServerTimings, SessionId, TermId, Token};

pub mod routes {
    pub const CLIENT: &str = "/v1/client";
    pub const HEALTH: &str = "/health";
    pub const PAIR_REDEEM: &str = "/api/pair/redeem";
    /// No body: the server asks tailscaled which machine is calling, and checks its `auto_pair`.
    pub const PAIR_AUTO: &str = "/api/pair/auto";
    /// A web page for a phone, with `?code=`: the link `psst-server pair` prints, and its QR code.
    pub const PAIR_PAGE: &str = "/pair";
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
    pub const POST_PROCESSING: &str = "/api/post-processing";
    pub const PROFILES_EXPORT: &str = "/api/profiles/export";
    /// Singular, so that every name (`export` too) has a path.
    pub const PROFILE: &str = "/api/profile/{name}";
    pub const PROFILE_RESET: &str = "/api/profile/{name}/reset";
    /// The server's settings: server.toml with the web UI's changes.
    pub const SETTINGS: &str = "/api/settings";
    pub const SETTINGS_EXPORT: &str = "/api/settings/export";
    /// Singular, as [`PROFILE`] is. `PUT` takes a [`SettingChange`](super::SettingChange).
    pub const SETTING: &str = "/api/setting/{key}";
    pub const SETTING_RESET: &str = "/api/setting/{key}/reset";
    /// A device's settings: its client.toml with the web UI's changes.
    pub const DEVICE_SETTINGS: &str = "/api/devices/{id}/settings";
    pub const DEVICE_SETTINGS_EXPORT: &str = "/api/devices/{id}/settings/export";
    pub const DEVICE_SETTING: &str = "/api/devices/{id}/setting/{key}";
    pub const DEVICE_SETTING_RESET: &str = "/api/devices/{id}/setting/{key}/reset";
    /// A paste rule, by Wayland app id. `PUT` takes an [`AppRuleView`](super::AppRuleView);
    /// `DELETE` hides the file's rule, or removes one added here.
    pub const DEVICE_APP: &str = "/api/devices/{id}/app/{app}";
    pub const DEVICE_APP_RESET: &str = "/api/devices/{id}/app/{app}/reset";
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

    /// Fills in `{placeholder}` with `value`, encoded.
    pub fn with(template: &str, placeholder: &str, value: &str) -> String {
        template.replace(&format!("{{{placeholder}}}"), &super::encode_segment(value))
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
    /// The server's best guess at how other devices reach it: its tailnet name, else its host
    /// name, at its TCP port. `None` without a TCP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Endpoint>,
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
    /// A change of model from the web UI, waiting for the dictations in progress to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<ModelReload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReload {
    /// The model it switches to.
    pub model: String,
    /// Dictations still in progress; the switch starts once there are none.
    pub waiting_for: u32,
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
    /// Connected now.
    #[serde(default, skip_serializing_if = "is_false")]
    pub online: bool,
    /// The device sent its config file when it last connected, so its settings can be edited.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reports_config: bool,
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

/// The post-processing styles a dictation can ask for, besides `none`: built-in ones first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostProcessingView {
    pub styles: Vec<String>,
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

/// Server or device settings, grouped as the web UI shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub groups: Vec<SettingsGroup>,
    /// Settings changed here, which the file doesn't have yet.
    pub changes: usize,
}

impl SettingsView {
    pub fn field(&self, key: &str) -> Option<&Setting> {
        self.groups.iter().flat_map(|group| &group.fields).find(|field| field.key == key)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsGroup {
    pub id: String,
    pub title: String,
    /// Tucked away: few need it, and a wrong value costs speed or accuracy.
    #[serde(default)]
    pub advanced: bool,
    pub fields: Vec<Setting>,
}

/// Values are JSON as the file's TOML has them, except a choice, which is always its `value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    /// Stable: the key's dotted path in the file, such as `limits.max_recording_s`.
    pub key: String,
    pub label: String,
    pub help: String,
    pub kind: SettingKind,
    /// In effect now; `null` for an optional key that's unset.
    pub value: serde_json::Value,
    /// The file's value, when it sets one other than the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<serde_json::Value>,
    pub default: serde_json::Value,
    /// The web UI changed it, so `value` is its change, unless `problem` says why not.
    pub changed_here: bool,
    pub access: SettingAccess,
    pub applies: Applies,
    /// Why the change made here is left out, so the file's value holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    Bool,
    Int {
        min: u64,
        max: u64,
        /// `s`, `ms`, `px`, `days`, or empty.
        unit: String,
    },
    Choice {
        options: Vec<SettingOption>,
        /// What a value outside `options` stands for, when one is allowed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other: Option<String>,
    },
    /// An optional one is unset when empty.
    Text {
        optional: bool,
    },
    /// `#rrggbb` or `#rrggbbaa`.
    Color,
    /// One item per line.
    List,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingAccess {
    Editable,
    /// Shown, never changed from the web UI.
    ReadOnly {
        reason: String,
    },
}

/// When a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    Now,
    NextDictation,
    /// When the device next connects; a connected one gets it now.
    NextConnect,
    /// The model reloads once no dictation is in progress.
    ModelReload,
    /// When psst-server next starts.
    NextStart,
}

/// The body of a setting's `PUT`: the value as a form field has it, parsed by the setting's kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingChange {
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsExport {
    /// The file with the web UI's changes in place: only the keys either sets.
    pub toml: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSettingsView {
    pub device: Device,
    /// What the device calls itself, as `name version`, when it last sent its config.
    pub client: String,
    pub reported_at: Timestamp,
    pub settings: SettingsView,
    /// Paste rules, by Wayland app id, sorted.
    pub apps: Vec<AppEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    pub app: String,
    pub layer: AppLayer,
    /// In effect: the changed one, or the file's; absent for a hidden rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<AppRuleView>,
    /// The file's, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<AppRuleView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppLayer {
    File,
    Replaced,
    Added,
    Hidden,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRuleView {
    /// `auto` (ctrl+shift+v in known terminals, else ctrl+v), `ctrl+v`, `ctrl+shift+v`, or `copy`
    /// to leave the text on the clipboard.
    pub paste: String,
    pub paste_delay_ms: u32,
}
