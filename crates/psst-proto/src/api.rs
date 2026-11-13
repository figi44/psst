//! HTTP routes and their bodies (plan 5.2), Phase 1. All bodies are JSON.
//!
//! Errors from `/api/*` come back with a non-2xx status and an [`ApiError`] body.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{ClientTimings, DeviceId, Final, HistoryId, ServerTimings, SessionId, Token};

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
