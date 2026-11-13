use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State, WebSocketUpgrade},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{AUTHORIZATION, COOKIE, HOST, ORIGIN},
        request::Parts,
    },
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use psst_dict::term_id;
use psst_pipeline::PipelineOptions;
use psst_proto::{
    ClientTimings, DeviceId, Endpoint, EngineResult, EngineTrace, Final, Segment, ServerTimings, Token,
    api::{
        ApiError, ApiErrorCode, Device, DictionaryExport, DictionaryTest, DictionaryView, Health, HistoryQuery,
        HistoryRow, LoginLink, ModelStatus, NeverRule, NeverRuleId, NewNeverRule, PairRedeem, PairRedeemed, Pairing,
        PairingRequest, Percentiles, PostProcessingView, Profile, ProfileChange, ProfilesExport, ProfilesView, Stats,
        Term, TermChange, routes,
    },
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    auth::{self, Identity, RedeemError, Refusal},
    conn,
    db::{Revoke, TailnetPairing, UNLOADED_BOOT_ID},
    dictionary::{self, Change},
    listen::Peer,
    profiles,
    state::AppState,
    tailscale,
};

type AppRef = Arc<AppState>;

const HISTORY_DEFAULT: u32 = 20;
const HISTORY_MAX: u32 = 1000;

pub fn router(state: AppRef) -> Router {
    Router::new()
        .route(routes::HEALTH, get(health))
        .route(routes::CLIENT, get(client))
        .route(routes::PAIR_REDEEM, post(pair_redeem))
        .route(routes::PAIR_AUTO, post(pair_auto))
        .route(routes::MODEL, get(model))
        .route(routes::MODEL_LOAD, post(model_load))
        .route(routes::MODEL_UNLOAD, post(model_unload))
        .route(routes::PAIRING, post(pairing))
        .route(routes::DEVICES, get(devices))
        .route(routes::DEVICE, delete(revoke))
        .route(routes::STATS, get(stats))
        .route(routes::HISTORY, get(history))
        .route(routes::DICTIONARY, get(dictionary_view))
        .route(routes::DICTIONARY_TERMS, post(add_term))
        .route(routes::DICTIONARY_TERM, put(replace_term).delete(remove_term))
        .route(routes::DICTIONARY_TERM_RESET, post(reset_term))
        .route(routes::DICTIONARY_NEVER, post(add_never))
        .route(routes::DICTIONARY_NEVER_RULE, delete(remove_never))
        .route(routes::DICTIONARY_EXPORT, get(export_dictionary))
        .route(routes::DICTIONARY_TEST, post(test_dictionary))
        .route(routes::PROFILES, get(profiles_view))
        .route(routes::POST_PROCESSING, get(post_processing_view))
        .route(routes::PROFILES_EXPORT, get(export_profiles))
        .route(routes::PROFILE, put(put_profile))
        .route(routes::PROFILE_RESET, post(reset_profile))
        .route(routes::LOGIN_LINK, post(login_link))
        .merge(crate::web::router())
        .fallback(not_found)
        .with_state(state)
}

async fn not_found(uri: axum::http::Uri) -> Response {
    if uri.path().starts_with("/api/") || uri.path().starts_with("/v1/") {
        ApiErr::new(ApiErrorCode::NotFound, "no such route").into_response()
    } else {
        crate::web::message(StatusCode::NOT_FOUND, "Not found", "There's no such page.", None)
    }
}

#[derive(Debug)]
pub struct ApiErr(ApiError);

impl ApiErr {
    pub(crate) fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self(ApiError { code, message: message.into() })
    }

    fn internal(err: anyhow::Error) -> Self {
        tracing::error!("{err:#}");
        Self::new(ApiErrorCode::Internal, "internal error; see the server's log")
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self.0.code {
            ApiErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            ApiErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ApiErrorCode::NotFound => StatusCode::NOT_FOUND,
            ApiErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ApiErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.0.message
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(err)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.status(), Json(self.0)).into_response()
    }
}

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

struct Id<T>(T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Id<T> {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiErr> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(id)| Id(id))
            .map_err(|err| ApiErr::new(ApiErrorCode::BadRequest, err.body_text()))
    }
}

pub(crate) fn peer(parts: &Parts) -> Peer {
    parts.extensions.get::<ConnectInfo<Peer>>().map(|info| info.0.clone()).unwrap_or(Peer::Unix { uid: None })
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    Identity(Identity),
    Browser {
        /// The SHA-256 of the session cookie.
        session: String,
        #[allow(dead_code)]
        device: DeviceId,
    },
}

pub(crate) fn authenticate(state: &AppState, parts: &Parts) -> Result<Caller, Refusal> {
    let peer = peer(parts);
    if matches!(peer, Peer::Tcp(_))
        && !parts.headers.contains_key(AUTHORIZATION)
        && let Some(cookie) = session_cookie(&parts.headers)
    {
        let session = auth::hash_token(cookie);
        let device = browser_session(state, &session)?;
        if !matches!(parts.method, Method::GET | Method::HEAD) && !same_origin(state, &parts.headers) {
            return Err(Refusal::CrossSite);
        }
        return Ok(Caller::Browser { session, device });
    }
    identify(state, &peer, &parts.headers).map(Caller::Identity)
}

pub(crate) fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(auth::SESSION_COOKIE)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

fn browser_session(state: &AppState, session: &str) -> Result<DeviceId, Refusal> {
    let device = match state.db.web_session(session) {
        Ok(Some(device)) => device,
        Ok(None) => return Err(Refusal::Unauthorized),
        Err(err) => {
            tracing::error!("cannot check a browser session: {err:#}");
            return Err(Refusal::Unauthorized);
        }
    };
    if device_allowed(state, &device) { Ok(device) } else { Err(Refusal::Unauthorized) }
}

pub(crate) fn device_allowed(state: &AppState, device: &str) -> bool {
    match device.strip_prefix("local:") {
        Some(user) => state.local.allows_user(user),
        None => state.db.device_active(device).unwrap_or_else(|err| {
            tracing::error!("cannot check device {device}: {err:#}");
            false
        }),
    }
}

pub(crate) fn same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|value: &HeaderValue| value.to_str().ok());
    let Some(origin) = header(ORIGIN) else {
        return header(HeaderName::from_static("sec-fetch-site")) == Some("same-origin");
    };
    let authority = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://"));
    let host = header(HOST);
    let same_host = authority.zip(host).is_some_and(|(authority, host)| authority.eq_ignore_ascii_case(host));
    let web_url = state.server.web_url.as_deref().and_then(psst_config::web_origin);
    same_host || web_url.is_some_and(|url| url.eq_ignore_ascii_case(origin))
}

/// Every authenticated caller is an admin: local users, paired devices and their browsers.
struct Admin(#[allow(dead_code)] Caller);

impl FromRequestParts<AppRef> for Admin {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, state: &AppRef) -> Result<Self, ApiErr> {
        authenticate(state, parts).map(Admin).map_err(refused)
    }
}

fn refused(refusal: Refusal) -> ApiErr {
    match refusal {
        Refusal::Unauthorized => {
            ApiErr::new(ApiErrorCode::Unauthorized, "missing or unknown device token, or an expired browser session")
        }
        Refusal::Forbidden => ApiErr::new(ApiErrorCode::Forbidden, "this user isn't in the server's local_users"),
        Refusal::CrossSite => {
            ApiErr::new(ApiErrorCode::Forbidden, "cross-site request refused: it must come from the web UI")
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

/// Only tailscaled says which machine is calling. Faking a machine's address doesn't fool it: on
/// tailscale0 only that machine's WireGuard key can send from the address, and the answer, token
/// included, goes back to the address.
async fn pair_auto(
    State(state): State<AppRef>,
    ConnectInfo(peer): ConnectInfo<Peer>,
) -> Result<Json<PairRedeemed>, ApiErr> {
    let Peer::Tcp(addr) = peer else {
        return Err(ApiErr::new(ApiErrorCode::NotFound, "this machine doesn't pair; it uses the socket"));
    };
    if !state.pairings.admit(addr.ip()) {
        return Err(ApiErr::new(ApiErrorCode::RateLimited, "too many pairing attempts; wait a minute"));
    }
    let forbidden = |message: String| Err(ApiErr::new(ApiErrorCode::Forbidden, message));
    if state.server.auto_pair.is_empty() {
        return forbidden("this server's auto_pair lists no machine; pair with a code from `psst-server pair`".into());
    }
    let machine = match tailscale::whois(&state.server.tailscale_socket, addr).await {
        Ok(Some(machine)) => machine,
        Ok(None) => return forbidden(format!("{} isn't a machine on the server's tailnet", addr.ip())),
        Err(err) => {
            tracing::warn!("cannot ask tailscaled who {addr} is: {err:#}");
            return forbidden("the server cannot ask tailscaled which machine this is; see the server's log".into());
        }
    };
    let name = machine.name;
    if !state.server.auto_pair.contains(&name) {
        return forbidden(format!("{name} isn't in the server's auto_pair"));
    }
    let token = auth::new_token();
    match state.db.add_tailnet_device(&name, &machine.id, &auth::hash_token(&token))? {
        TailnetPairing::Paired { id, replaced } => {
            if let Some(replaced) = replaced {
                tracing::info!("revoked device {replaced}, which {name} paired as but never connected with");
                // One may be connecting right now, registered but not yet counted as seen.
                state.conns.close(&replaced, "the device was paired again");
            }
            tracing::info!("paired device {id} ({name}, tailnet machine {}) from {addr}", machine.id);
            Ok(Json(PairRedeemed { device_id: id, token: Token::new(token) }))
        }
        TailnetPairing::Taken(live) => forbidden(format!(
            "{name} is already paired as device {live}; if it lost its token, run `psst-server devices revoke {live}` on the server"
        )),
    }
}

/// Never from a browser session, so a session can't extend itself.
async fn login_link(
    State(state): State<AppRef>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
) -> Result<Json<LoginLink>, ApiErr> {
    if matches!(peer, Peer::Tcp(_)) && !headers.contains_key(AUTHORIZATION) && session_cookie(&headers).is_some() {
        return Err(ApiErr::new(
            ApiErrorCode::Forbidden,
            "a browser session can't ask for login links; run `psst ui` on a paired machine",
        ));
    }
    let identity = identify(&state, &peer, &headers).map_err(refused)?;
    let base = link_base(&state, &peer, &headers)?;
    let token = state.logins.create(identity.device(), std::time::Instant::now());
    let expires_at = jiff::Timestamp::now() + auth::LINK_TTL;
    let expires_at = expires_at.round(jiff::Unit::Second).unwrap_or(expires_at);
    tracing::info!("login link for {} created; valid until {expires_at}", identity.device());
    Ok(Json(LoginLink { url: format!("{base}/login?token={token}"), expires_at }))
}

fn link_base(state: &AppState, peer: &Peer, headers: &HeaderMap) -> Result<String, ApiErr> {
    if let Some(url) = state.server.web_url.as_deref().and_then(psst_config::web_origin) {
        return Ok(url.to_owned());
    }
    if let Peer::Tcp(_) = peer {
        let host = headers.get(HOST).and_then(|host| host.to_str().ok()).unwrap_or_default();
        let base = format!("http://{host}");
        return match psst_config::web_origin(&base) {
            Some(base) => Ok(base.to_owned()),
            None => Err(ApiErr::new(ApiErrorCode::BadRequest, format!("cannot build a link for Host `{host}`"))),
        };
    }
    let loopback = |addr: &std::net::SocketAddr| match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() || ip.is_loopback() => {
            Some(format!("http://127.0.0.1:{}", addr.port()))
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() || ip.is_loopback() => {
            Some(format!("http://[::1]:{}", addr.port()))
        }
        _ => None,
    };
    state
        .tcp_addrs
        .iter()
        .find_map(loopback)
        .or_else(|| state.tcp_addrs.first().map(|addr| format!("http://{addr}")))
        .ok_or_else(|| {
            ApiErr::new(
                ApiErrorCode::BadRequest,
                "the server has no TCP listener (listen = []), and browsers reach the web UI only over TCP",
            )
        })
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
    Ok(Json(Pairing { server: own_address(&state).await, ..pairing }))
}

/// A name, not an IP address: a phone keeps it while DHCP or Tailscale moves the server.
async fn own_address(state: &AppState) -> Option<Endpoint> {
    let port = state.tcp_addrs.first()?.port();
    let host = match tailscale::own_name(&state.server.tailscale_socket).await {
        Ok(Some(name)) => name,
        Ok(None) => host_name()?,
        Err(err) => {
            tracing::debug!("no tailnet name from tailscaled: {err:#}");
            host_name()?
        }
    };
    Some(Endpoint::Tcp { host, port })
}

fn host_name() -> Option<String> {
    nix::unistd::gethostname().ok()?.into_string().ok().filter(|name| !name.is_empty())
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
    Ok(Json(state.db.history(limit, query.device.as_deref())?))
}

async fn dictionary_view(_: Admin, State(state): State<AppRef>) -> Json<DictionaryView> {
    let layered = state.live.layered();
    Json(layered.view(&state.live.dictionary()))
}

async fn add_term(_: Admin, State(state): State<AppRef>, Body(term): Body<Term>) -> Result<Json<TermChange>, ApiErr> {
    dictionary::change_term(&state.live, &state.db, Change::Add(term.into())).map(Json)
}

async fn replace_term(
    _: Admin,
    State(state): State<AppRef>,
    Id(id): Id<String>,
    Body(term): Body<Term>,
) -> Result<Json<TermChange>, ApiErr> {
    dictionary::change_term(&state.live, &state.db, Change::Replace(term_id(&id), term.into())).map(Json)
}

async fn remove_term(_: Admin, State(state): State<AppRef>, Id(id): Id<String>) -> Result<Json<TermChange>, ApiErr> {
    dictionary::change_term(&state.live, &state.db, Change::Remove(term_id(&id))).map(Json)
}

async fn reset_term(_: Admin, State(state): State<AppRef>, Id(id): Id<String>) -> Result<Json<TermChange>, ApiErr> {
    dictionary::change_term(&state.live, &state.db, Change::Reset(term_id(&id))).map(Json)
}

async fn add_never(
    _: Admin,
    State(state): State<AppRef>,
    Body(rule): Body<NewNeverRule>,
) -> Result<Json<NeverRule>, ApiErr> {
    dictionary::add_never(&state.live, &state.db, &rule.term, &rule.span).map(Json)
}

async fn remove_never(_: Admin, State(state): State<AppRef>, Id(id): Id<NeverRuleId>) -> Result<StatusCode, ApiErr> {
    dictionary::remove_never(&state.live, &state.db, id).map(|()| StatusCode::NO_CONTENT)
}

async fn export_dictionary(_: Admin, State(state): State<AppRef>) -> Json<DictionaryExport> {
    Json(DictionaryExport { toml: state.live.layered().export() })
}

async fn test_dictionary(
    _: Admin,
    State(state): State<AppRef>,
    Body(test): Body<DictionaryTest>,
) -> Result<Json<Final>, ApiErr> {
    if test.language.as_deref().is_some_and(|language| language.trim().is_empty()) {
        return Err(ApiErr::new(ApiErrorCode::BadRequest, "language must be a language code"));
    }
    let profile = state
        .live
        .default_profile_in(test.language.as_deref())
        .ok_or_else(|| ApiErr::internal(anyhow::anyhow!("profiles.toml has no default profile")))?;
    let segment = Segment { text: test.text, start_ms: 0, end_ms: 0, no_speech_prob: None, words: None };
    let trace = EngineTrace {
        engine: "dictionary test".into(),
        model: String::new(),
        revision: String::new(),
        path: String::new(),
        options: Value::Null,
        prompt: String::new(),
        result: EngineResult { language: profile.language.clone(), speech_ms: None, segments: vec![segment] },
    };
    let dictionary = state.live.dictionary();
    Ok(Json(psst_pipeline::run(&trace, &profile, &dictionary, &PipelineOptions::default())))
}

async fn post_processing_view(_: Admin, State(state): State<AppRef>) -> Json<PostProcessingView> {
    Json(PostProcessingView { styles: crate::tidy::entries(&state).into_iter().map(|entry| entry.id).collect() })
}

async fn profiles_view(_: Admin, State(state): State<AppRef>) -> Json<ProfilesView> {
    Json(state.live.profile_layers().view())
}

async fn export_profiles(_: Admin, State(state): State<AppRef>) -> Json<ProfilesExport> {
    Json(ProfilesExport { toml: state.live.profile_layers().export() })
}

async fn put_profile(
    _: Admin,
    State(state): State<AppRef>,
    Id(name): Id<String>,
    Body(profile): Body<Profile>,
) -> Result<Json<ProfileChange>, ApiErr> {
    profiles::change(&state.live, &state.db, &name, Some(profile)).map(Json)
}

async fn reset_profile(
    _: Admin,
    State(state): State<AppRef>,
    Id(name): Id<String>,
) -> Result<Json<ProfileChange>, ApiErr> {
    profiles::change(&state.live, &state.db, &name, None).map(Json)
}

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

/// Nearest-rank; `sorted` must be non-empty.
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
