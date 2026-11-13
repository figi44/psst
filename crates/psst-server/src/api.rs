//! HTTP routes (plan 5.2): `/health`, the client WebSocket, pairing and the Phase 1 `/api`.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use psst_proto::{
    ClientTimings, ServerTimings, Token,
    api::{
        ApiError, ApiErrorCode, Device, Health, HistoryQuery, HistoryRow, ModelStatus, PairRedeem, PairRedeemed,
        Pairing, PairingRequest, Percentiles, Stats, routes,
    },
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    auth::{self, Identity, RedeemError, Refusal},
    conn,
    db::{Revoke, UNLOADED_BOOT_ID},
    listen::Peer,
    state::AppState,
};

type AppRef = Arc<AppState>;

/// History rows returned when `limit` is absent, and the most ever returned.
const HISTORY_DEFAULT: u32 = 20;
const HISTORY_MAX: u32 = 1000;

pub fn router(state: AppRef) -> Router {
    Router::new()
        .route(routes::HEALTH, get(health))
        .route(routes::CLIENT, get(client))
        .route(routes::PAIR_REDEEM, post(pair_redeem))
        .route(routes::MODEL, get(model))
        .route(routes::MODEL_LOAD, post(model_load))
        .route(routes::MODEL_UNLOAD, post(model_unload))
        .route(routes::PAIRING, post(pairing))
        .route(routes::DEVICES, get(devices))
        .route(routes::DEVICE, delete(revoke))
        .route(routes::STATS, get(stats))
        .route(routes::HISTORY, get(history))
        .fallback(async || ApiErr::new(ApiErrorCode::NotFound, "no such route"))
        .with_state(state)
}

/// An `/api` error: a status and an [`ApiError`] body.
#[derive(Debug)]
pub struct ApiErr(ApiError);

impl ApiErr {
    fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self(ApiError { code, message: message.into() })
    }

    fn internal(err: anyhow::Error) -> Self {
        tracing::error!("{err:#}");
        Self::new(ApiErrorCode::Internal, "internal error; see the server's log")
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(err)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ApiErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            ApiErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ApiErrorCode::NotFound => StatusCode::NOT_FOUND,
            ApiErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ApiErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

/// A JSON body whose rejections are [`ApiErr`]s.
struct Body<T>(T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiErr;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiErr> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(body)| Body(body))
            .map_err(|err| ApiErr::new(ApiErrorCode::BadRequest, err.body_text()))
    }
}

fn peer(parts: &Parts) -> Peer {
    parts.extensions.get::<ConnectInfo<Peer>>().map(|info| info.0.clone()).unwrap_or(Peer::Unix { uid: None })
}

/// Who is calling: a local user by peer credentials, or a paired device by its bearer token.
fn identify(state: &AppState, peer: &Peer, headers: &HeaderMap) -> Result<Identity, Refusal> {
    match peer {
        Peer::Unix { uid } => state.local.identify(*uid),
        Peer::Tcp(_) => {
            let token = headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or(Refusal::Unauthorized)?;
            match state.db.device_by_token(&auth::hash_token(token.trim())) {
                Ok(Some(device)) => Ok(Identity::Token { device_id: device.id }),
                Ok(None) => Err(Refusal::Unauthorized),
                Err(err) => {
                    tracing::error!("cannot check a token: {err:#}");
                    Err(Refusal::Unauthorized)
                }
            }
        }
    }
}

/// An authenticated `/api` caller. Local users are admins; so are paired devices (plan 5.2).
struct Admin(#[allow(dead_code)] Identity);

impl FromRequestParts<AppRef> for Admin {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, state: &AppRef) -> Result<Self, ApiErr> {
        match identify(state, &peer(parts), &parts.headers) {
            Ok(identity) => Ok(Admin(identity)),
            Err(Refusal::Unauthorized) => {
                Err(ApiErr::new(ApiErrorCode::Unauthorized, "missing or unknown device token"))
            }
            Err(Refusal::Forbidden) => {
                Err(ApiErr::new(ApiErrorCode::Forbidden, "this user isn't in the server's local_users"))
            }
        }
    }
}

async fn health() -> Json<Health> {
    Json(Health { version: env!("CARGO_PKG_VERSION").into() })
}

async fn client(
    State(state): State<AppRef>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let identity = identify(&state, &peer, &headers);
    ws.on_upgrade(move |socket| conn::run(state, socket, identity))
}

async fn pair_redeem(
    State(state): State<AppRef>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    Body(req): Body<PairRedeem>,
) -> Result<Json<PairRedeemed>, ApiErr> {
    let Peer::Tcp(addr) = peer else {
        return Err(ApiErr::new(ApiErrorCode::NotFound, "this machine doesn't pair; it uses the socket"));
    };
    let name = match state.pairings.redeem(addr.ip(), &req.code) {
        Ok(name) => name,
        Err(RedeemError::RateLimited) => {
            return Err(ApiErr::new(ApiErrorCode::RateLimited, "too many pairing attempts; wait a minute"));
        }
        Err(RedeemError::Invalid) => {
            return Err(ApiErr::new(ApiErrorCode::Forbidden, "unknown, expired or void pairing code"));
        }
    };
    let token = auth::new_token();
    let device_id = state.db.add_token_device(&name, &auth::hash_token(&token))?;
    tracing::info!("paired device {device_id} ({name}, which calls itself {:?}) from {addr}", req.name);
    Ok(Json(PairRedeemed { device_id, token: Token::new(token) }))
}

async fn model(_: Admin, State(state): State<AppRef>) -> Json<ModelStatus> {
    Json(state.engine.status())
}

async fn model_load(_: Admin, State(state): State<AppRef>) -> Result<Json<ModelStatus>, ApiErr> {
    state.db.clear_state(UNLOADED_BOOT_ID)?;
    Ok(Json(state.engine.load().await))
}

async fn model_unload(_: Admin, State(state): State<AppRef>) -> Result<Json<ModelStatus>, ApiErr> {
    state.db.set_state(UNLOADED_BOOT_ID, &crate::boot_id())?;
    Ok(Json(state.engine.unload().await))
}

async fn pairing(
    _: Admin,
    State(state): State<AppRef>,
    Body(req): Body<PairingRequest>,
) -> Result<Json<Pairing>, ApiErr> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiErr::new(ApiErrorCode::BadRequest, "the device needs a name"));
    }
    let pairing = state.pairings.create(name.to_owned());
    tracing::info!("pairing code for {name} created; valid until {}", pairing.expires_at);
    Ok(Json(pairing))
}

async fn devices(_: Admin, State(state): State<AppRef>) -> Result<Json<Vec<Device>>, ApiErr> {
    Ok(Json(state.db.devices()?))
}

async fn revoke(_: Admin, State(state): State<AppRef>, Path(id): Path<String>) -> Result<StatusCode, ApiErr> {
    match state.db.revoke(&id)? {
        Revoke::Revoked => {
            tracing::info!("revoked device {id}");
            state.conns.close(&id, "the device was revoked");
            Ok(StatusCode::NO_CONTENT)
        }
        Revoke::NotFound => Err(ApiErr::new(ApiErrorCode::NotFound, format!("no device {id}"))),
        Revoke::Local => Err(ApiErr::new(
            ApiErrorCode::BadRequest,
            format!("{id} is a local user; remove it from local_users in server.toml instead"),
        )),
    }
}

async fn stats(_: Admin, State(state): State<AppRef>) -> Result<Json<Stats>, ApiErr> {
    Ok(Json(compute_stats(&state.db.timings()?)))
}

async fn history(
    _: Admin,
    State(state): State<AppRef>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<HistoryRow>>, ApiErr> {
    let Query(query) = query.map_err(|err| ApiErr::new(ApiErrorCode::BadRequest, err.body_text()))?;
    let limit = query.limit.unwrap_or(HISTORY_DEFAULT).min(HISTORY_MAX);
    Ok(Json(state.db.history(limit)?))
}

/// p50/p95 per timing field (plan 6.7). `network_estimate_ms` is the client's stop→final minus
/// the server's.
pub fn compute_stats(rows: &[(ServerTimings, Option<ClientTimings>)]) -> Stats {
    let mut fields: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut add = |prefix: &str, value: Value| {
        if let Value::Object(map) = value {
            for (key, value) in map {
                if let Some(value) = value.as_u64() {
                    fields.entry(format!("{prefix}{key}")).or_default().push(value.try_into().unwrap_or(u32::MAX));
                }
            }
        }
    };
    let mut network = Vec::new();
    for (server, client) in rows {
        add("server.", serde_json::to_value(server).expect("timings serialise"));
        if let Some(client) = client {
            add("client.", serde_json::to_value(client).expect("timings serialise"));
            if let Some(client_ms) = client.stop_to_final_ms {
                network.push(client_ms.saturating_sub(server.stop_to_final_ms));
            }
        }
    }
    if !network.is_empty() {
        fields.insert("network_estimate_ms".into(), network);
    }
    let timings = fields
        .into_iter()
        .map(|(key, mut values)| {
            values.sort_unstable();
            let percentiles =
                Percentiles { count: values.len() as u64, p50: percentile(&values, 50), p95: percentile(&values, 95) };
            (key, percentiles)
        })
        .collect();
    Stats { sessions: rows.len() as u64, timings }
}

/// Nearest-rank percentile of sorted, non-empty values.
fn percentile(sorted: &[u32], pct: usize) -> u32 {
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(stop_to_final_ms: u32, engine_ms: u32) -> ServerTimings {
        ServerTimings { stop_to_final_ms, engine_ms, attempts: 1, ..ServerTimings::default() }
    }

    #[test]
    fn stats_give_nearest_rank_percentiles_per_field() {
        let rows: Vec<_> = (1..=20).map(|i| (server(i * 10, i), None)).collect();
        let stats = compute_stats(&rows);
        assert_eq!(stats.sessions, 20);
        assert_eq!(stats.timings["server.stop_to_final_ms"], Percentiles { count: 20, p50: 100, p95: 190 });
        assert_eq!(stats.timings["server.engine_ms"], Percentiles { count: 20, p50: 10, p95: 19 });
        assert!(!stats.timings.contains_key("network_estimate_ms"));
    }

    #[test]
    fn stats_estimate_the_network_from_client_reports() {
        let client =
            ClientTimings { stop_to_final_ms: Some(130), stop_to_dispatch_ms: Some(140), ..Default::default() };
        let rows = vec![(server(100, 80), Some(client)), (server(100, 80), None)];
        let stats = compute_stats(&rows);
        assert_eq!(stats.timings["network_estimate_ms"], Percentiles { count: 1, p50: 30, p95: 30 });
        assert_eq!(stats.timings["client.stop_to_dispatch_ms"].count, 1);
        assert_eq!(stats.timings["server.audio_ms"].count, 2);
    }
}
