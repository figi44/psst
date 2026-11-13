//! Sign in with ChatGPT and the Responses API on the user's ChatGPT plan, as OpenAI documents it
//! for open-source apps: https://developers.openai.com/siwc/token-sharing-open-source

use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{
    Method, StatusCode,
    body::Bytes,
    header::{self, HeaderName},
};
use jiff::Timestamp;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::AbortHandle,
};
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Error as WsError, Message, client::IntoClientRequest},
};

use crate::state::AppState;

pub const MANAGE_USAGE: &str = "https://chatgpt.com/settings/usage";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// Also the `resource` the tokens are for.
const API: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const REGISTER: &str = "dynamic_agent_client";
const APP_NAME: &str = "psst";
/// Only the port may change between sign-ins; this is the one OpenAI's examples use.
const PORT: u16 = 1455;
const SIGN_IN_WINDOW: Duration = Duration::from_secs(600);
const REFRESH_MARGIN_S: i64 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const MODELS_FRESH: Duration = Duration::from_secs(600);
const RELOGIN_CODES: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];
const HOST_KEY: &str = "tidy.chatgpt.host";
const CREDENTIALS_KEY: &str = "tidy.chatgpt.credentials";

const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// What identifies this server to OpenAI. It outlives a disconnect, as the docs require.
#[derive(Serialize, Deserialize)]
struct Host {
    ext_agent_host_id: String,
    client_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    subject: String,
    client_id: String,
    id_token: String,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

pub struct Pending {
    pub url: String,
    /// Whether this server waits on the redirect address itself, which reaches it only when the
    /// browser runs on the same machine.
    pub listening: bool,
    csrf: String,
    verifier: String,
    nonce: String,
    redirect_uri: String,
    back: Option<String>,
    listener: Option<AbortHandle>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending").field("listening", &self.listening).finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct SignIn {
    pub pending: Option<Pending>,
    /// Bumped by a cancel or a disconnect: a sign-in already past taking `pending` saves its
    /// credentials only if this hasn't changed since.
    generation: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub slug: String,
    pub name: String,
}

pub struct Ask<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub instructions: &'a str,
    pub text: &'a str,
    /// Merged into the request body last, to try fields the lab has no knob for.
    pub extra: Option<&'a Value>,
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    pub first_token: Option<Duration>,
    pub total: Duration,
    /// Opening the socket first, which a run on an already-open one skips.
    pub connect: Option<Duration>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn usage_limit(&self) -> bool {
        self.code.starts_with("subscription_sharing_usage")
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

pub fn credentials(state: &AppState) -> Option<Credentials> {
    let text = state.db.get_state(CREDENTIALS_KEY).ok().flatten()?;
    serde_json::from_str(&text).ok()
}

pub async fn disconnect(state: &AppState) -> Result<()> {
    // A refresh in flight would save its tokens after this, and sign the plan back in; so would a
    // sign-in in its token exchange.
    let _no_refresh = state.tidy.refresh.lock().await;
    state.tidy.sign_in().generation += 1;
    forget(state)
}

fn forget(state: &AppState) -> Result<()> {
    state.db.clear_state(CREDENTIALS_KEY)?;
    *state.tidy.models() = None;
    Ok(())
}

/// `back` is where the browser returns after a sign-in this server receives itself.
pub async fn start(state: &Arc<AppState>, back: Option<String>) -> Result<()> {
    cancel(state);
    let host = host(state)?;
    let verifier = random_token(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let csrf = random_token(16);
    let nonce = random_token(16);
    let listener = match TcpListener::bind(("127.0.0.1", PORT)).await {
        Ok(listener) => Some(listener),
        Err(_) => TcpListener::bind(("127.0.0.1", 0)).await.ok(),
    };
    let port = listener.as_ref().and_then(|l| l.local_addr().ok()).map_or(PORT, |addr| addr.port());
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");

    let previous = credentials(state);
    let mut params = Vec::new();
    match &host.client_id {
        Some(id) => {
            params.push(("client_id", id.as_str()));
            if let Some(previous) = &previous {
                params.push(("id_token_hint", previous.id_token.as_str()));
            }
        }
        None => params.extend([("client_id", REGISTER), ("agent_name_hint", APP_NAME)]),
    }
    params.extend([
        ("ext_agent_host_id", host.ext_agent_host_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", &redirect_uri),
        ("scope", SCOPE),
        ("resource", API),
        ("state", &csrf),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    let url = format!("{AUTHORIZE}?{}", encode_form(&params));

    let listening = listener.is_some();
    let listener = listener.map(|l| tokio::spawn(listen(state.clone(), l, csrf.clone())).abort_handle());
    state.tidy.sign_in().pending =
        Some(Pending { url, listening, csrf, verifier, nonce, redirect_uri, back, listener });
    Ok(())
}

pub fn cancel(state: &AppState) {
    let mut sign_in = state.tidy.sign_in();
    sign_in.generation += 1;
    if let Some(pending) = sign_in.pending.take()
        && let Some(listener) = pending.listener
    {
        listener.abort();
    }
}

/// `pasted` is the address the sign-in ended on, or just its query.
pub async fn paste(state: &AppState, pasted: &str) -> Result<String> {
    let query = pasted.trim().split_once('?').map_or(pasted.trim(), |(_, query)| query);
    let params = parse_query(query.split('#').next().unwrap_or_default());
    finish(state, &params, true).await
}

/// The listener finishes its own sign-in, so it mustn't be stopped from within.
async fn finish(state: &AppState, params: &HashMap<String, String>, stop_listener: bool) -> Result<String> {
    let (pending, generation) = {
        let mut sign_in = state.tidy.sign_in();
        match sign_in.pending.as_ref() {
            None => bail!("No sign-in is waiting. Start one with Continue with ChatGPT."),
            Some(p) if params.get("state") != Some(&p.csrf) => {
                bail!("That address is from another sign-in. Paste the one this sign-in ended on.")
            }
            Some(_) => (sign_in.pending.take().expect("checked above"), sign_in.generation),
        }
    };
    if stop_listener && let Some(listener) = &pending.listener {
        listener.abort();
    }
    if let Some(error) = params.get("error") {
        let detail = params.get("error_description").map(String::as_str).unwrap_or_default();
        bail!("ChatGPT refused the sign-in: {error} {detail}");
    }
    let code = params.get("code").ok_or_else(|| anyhow!("The address has no sign-in code. Copy all of it."))?;
    let mut host = host(state)?;
    let client_id = match (params.get("client_id"), &host.client_id) {
        (Some(new), Some(saved)) if new != saved => {
            bail!("OpenAI issued client ID {new}, but this server registered as {saved}")
        }
        (Some(new), _) => new.clone(),
        (None, Some(saved)) => saved.clone(),
        (None, None) => bail!("The sign-in carried no client ID for psst"),
    };

    let (status, body) = post_form(
        state,
        TOKEN,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", code),
            ("code_verifier", &pending.verifier),
            ("redirect_uri", &pending.redirect_uri),
            ("resource", API),
        ],
    )
    .await?;
    let tokens = token_response(status, &body)?;
    let id_token = tokens.id_token.clone().ok_or_else(|| anyhow!("OpenAI sent no ID token"))?;
    // The ID token came straight from the token endpoint over TLS, which OpenID Connect accepts
    // in place of checking its signature; the claims still have to match this sign-in.
    let claims = jwt_claims(&id_token)?;
    if claims["nonce"].as_str() != Some(pending.nonce.as_str()) {
        bail!("The ID token doesn't belong to this sign-in");
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => *aud == client_id,
        Value::Array(auds) => auds.iter().any(|a| a.as_str() == Some(client_id.as_str())),
        _ => false,
    };
    if !audience_ok {
        bail!("The ID token was issued to another app");
    }
    let scopes = scopes(tokens.scope.as_deref().or(params.get("scope").map(String::as_str)));
    if !scopes.iter().any(|s| s == PLAN_SCOPE) {
        bail!("Signed in, but ChatGPT didn't grant plan usage: the plan may not be eligible (Plus or Pro)");
    }

    host.client_id = Some(client_id.clone());
    save(state, HOST_KEY, &host)?;
    let creds = Credentials {
        email: claims["email"].as_str().unwrap_or_default().to_owned(),
        subject: claims["sub"].as_str().unwrap_or_default().to_owned(),
        client_id,
        id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.ok_or_else(|| anyhow!("OpenAI sent no refresh token"))?,
        expires_at: Timestamp::now().as_second() + tokens.expires_in.unwrap_or(3600),
        scopes,
    };
    let _no_refresh = state.tidy.refresh.lock().await;
    {
        let sign_in = state.tidy.sign_in();
        if sign_in.generation != generation {
            bail!("The sign-in was cancelled.");
        }
        save(state, CREDENTIALS_KEY, &creds)?;
    }
    *state.tidy.models() = None;
    tracing::info!("post-processing: signed in to ChatGPT as {}", creds.email);
    Ok(creds.email)
}

async fn listen(state: Arc<AppState>, listener: TcpListener, csrf: String) {
    let window = tokio::time::sleep(SIGN_IN_WINDOW);
    tokio::pin!(window);
    loop {
        let mut stream = tokio::select! {
            _ = &mut window => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let Some(target) = request_target(&mut stream).await else { continue };
        let Some(query) = target.strip_prefix("/auth/callback") else {
            reply(&mut stream, "404 Not Found", None, "Not found.", None).await;
            continue;
        };
        let params = parse_query(query.trim_start_matches('?'));
        if params.get("state") != Some(&csrf) {
            reply(&mut stream, "400 Bad Request", None, "This sign-in was replaced by a newer one.", None).await;
            continue;
        }
        let back = state.tidy.sign_in().pending.as_ref().and_then(|p| p.back.clone());
        match finish(&state, &params, false).await {
            Ok(email) => match &back {
                Some(back) => reply(&mut stream, "303 See Other", Some(back), "Signed in.", None).await,
                None => {
                    let text = format!("psst is signed in to ChatGPT as {email}. You can close this tab.");
                    reply(&mut stream, "200 OK", None, &text, None).await
                }
            },
            Err(err) => {
                let text = format!("Signing psst in to ChatGPT failed: {err:#}");
                reply(&mut stream, "200 OK", None, &text, back.as_deref()).await
            }
        }
        return;
    }
    let mut sign_in = state.tidy.sign_in();
    if sign_in.pending.as_ref().is_some_and(|p| p.csrf == csrf) {
        sign_in.pending = None;
    }
}

async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0; 2048];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(10), read).await.ok()??;
    let head = String::from_utf8_lossy(&head);
    head.lines().next()?.split_whitespace().nth(1).map(str::to_owned)
}

async fn reply(stream: &mut TcpStream, status: &str, location: Option<&str>, text: &str, back: Option<&str>) {
    let link = back.map(|back| format!(" <a href=\"{}\">Back to psst</a>", escape(back))).unwrap_or_default();
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>psst</title>\
         <p>{}{link}</p>",
        escape(text)
    );
    let location = location.map(|l| format!("Location: {l}\r\n")).unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\n{location}Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// On a task of its own: a caller that gives up, such as a dictation out of time, mustn't stop a
/// refresh between OpenAI rotating the refresh token and the replacement being saved.
async fn access_token(state: &Arc<AppState>, force_refresh: bool) -> Result<String> {
    let state = state.clone();
    tokio::spawn(async move { current_token(&state, force_refresh).await }).await?
}

async fn current_token(state: &AppState, force_refresh: bool) -> Result<String> {
    // Refresh tokens rotate: two refreshes at once would spend the same one twice.
    let _one_at_a_time = state.tidy.refresh.lock().await;
    let mut creds = credentials(state).ok_or_else(|| anyhow!("Connect your ChatGPT plan first."))?;
    if !force_refresh && Timestamp::now().as_second() + REFRESH_MARGIN_S < creds.expires_at {
        return Ok(creds.access_token);
    }
    let (status, body) = post_form(
        state,
        TOKEN,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &creds.client_id),
            ("refresh_token", &creds.refresh_token),
            ("resource", API),
        ],
    )
    .await?;
    let tokens = match token_response(status, &body) {
        Ok(tokens) => tokens,
        Err(err) if err.downcast_ref::<ApiError>().is_some_and(|e| RELOGIN_CODES.contains(&e.code.as_str())) => {
            forget(state)?;
            bail!("Your ChatGPT sign-in expired. Connect again.");
        }
        Err(err) => return Err(err),
    };
    creds.access_token = tokens.access_token;
    if let Some(refresh) = tokens.refresh_token {
        creds.refresh_token = refresh;
    }
    if let Some(id) = tokens.id_token {
        creds.id_token = id;
    }
    creds.expires_at = Timestamp::now().as_second() + tokens.expires_in.unwrap_or(3600);
    if tokens.scope.is_some() {
        creds.scopes = scopes(tokens.scope.as_deref());
    }
    save(state, CREDENTIALS_KEY, &creds)?;
    Ok(creds.access_token)
}

pub async fn models(state: &Arc<AppState>) -> Result<Vec<Model>> {
    if let Some((at, models)) = state.tidy.models().clone()
        && at.elapsed() < MODELS_FRESH
    {
        return Ok(models);
    }
    let token = access_token(state, false).await?;
    let (status, body) = call(state, Method::GET, &format!("{API}/models"), &token, Bytes::new()).await?;
    if !status.is_success() {
        return Err(http_error(status, &body).into());
    }
    let value: Value = serde_json::from_slice(&body).context("OpenAI's model list didn't parse")?;
    let list = value["models"].as_array().or(value["data"].as_array()).cloned().unwrap_or_default();
    let models: Vec<Model> = list
        .iter()
        .filter(|m| m["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|m| {
            let slug = m["slug"].as_str().or(m["id"].as_str())?.to_owned();
            let name = m["display_name"].as_str().unwrap_or(&slug).to_owned();
            Some(Model { slug, name })
        })
        .collect();
    *state.tidy.models() = Some((Instant::now(), models.clone()));
    Ok(models)
}

/// One kept-open socket. Plan usage pays a ~1 s admission per HTTP request, but only once per
/// socket, when it opens.
pub struct Socket {
    ws: WebSocketStream<TlsStream<TcpStream>>,
    token: String,
    opened: Instant,
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socket").field("opened", &self.opened).finish_non_exhaustive()
    }
}

/// OpenAI closes sockets at 60 minutes.
const SOCKET_AGE: Duration = Duration::from_secs(55 * 60);

impl Socket {
    fn fresh_for(&self, token: &str) -> bool {
        self.token == token && self.opened.elapsed() < SOCKET_AGE
    }
}

enum Open {
    Unauthorized,
    Failed(anyhow::Error),
}

enum Exchange {
    /// OpenAI ended the response with an error; the socket is ready for the next one.
    Failed(anyhow::Error),
    /// The socket broke; `retry` when it broke before OpenAI said anything about this request.
    Broken { err: anyhow::Error, retry: bool },
}

pub async fn respond(state: &Arc<AppState>, ask: &Ask<'_>) -> Result<Answer> {
    let mut event = json!({
        "type": "response.create",
        "model": ask.model,
        "instructions": ask.instructions,
        "input": [{"role": "user", "content": ask.text}],
        "store": false,
    });
    if let Some(effort) = ask.effort {
        event["reasoning"] = json!({ "effort": effort });
    }
    if let Some(tier) = ask.tier {
        event["service_tier"] = json!(tier);
    }
    if let Some(Value::Object(extra)) = ask.extra {
        for (key, value) in extra {
            event[key] = value.clone();
        }
    }
    let event = event.to_string();
    let (mut refreshed, mut reopened) = (false, false);
    loop {
        let token = access_token(state, refreshed).await?;
        let mut slot = state.tidy.socket.lock().await;
        let mut connect = None;
        // Out of the cache while in use: a caller that gives up mid-answer drops it, unread answer
        // and all, so the next request can't take that answer for its own.
        let mut socket = match slot.take() {
            Some(socket) if socket.fresh_for(&token) => socket,
            _ => {
                let opening = Instant::now();
                match open(state, &token).await {
                    Ok(socket) => {
                        connect = Some(opening.elapsed());
                        socket
                    }
                    Err(Open::Unauthorized) if !refreshed => {
                        refreshed = true;
                        continue;
                    }
                    Err(Open::Unauthorized) => bail!("OpenAI refused the sign-in. Connect your ChatGPT plan again."),
                    Err(Open::Failed(err)) => return Err(err),
                }
            }
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, turn(&mut socket, &event)).await {
            Ok(Ok(mut answer)) => {
                *slot = Some(socket);
                answer.connect = connect;
                return Ok(answer);
            }
            Ok(Err(Exchange::Failed(err))) => {
                *slot = Some(socket);
                return Err(err);
            }
            Ok(Err(Exchange::Broken { err, retry })) => {
                // OpenAI may have closed an idle socket; one fresh socket tries the request again.
                if !retry || reopened || connect.is_some() {
                    return Err(err);
                }
                reopened = true;
            }
            Err(_) => bail!("OpenAI didn't finish within {REQUEST_TIMEOUT:?}"),
        }
    }
}

/// Opens the socket ahead of a run, so the first run doesn't wait for the admission.
pub fn warm(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        let Ok(token) = access_token(&state, false).await else { return };
        let mut slot = state.tidy.socket.lock().await;
        if slot.as_ref().is_some_and(|socket| socket.fresh_for(&token)) {
            return;
        }
        match open(&state, &token).await {
            Ok(socket) => *slot = Some(socket),
            Err(Open::Failed(err)) => tracing::debug!("tidy-up lab: cannot open a socket: {err:#}"),
            Err(Open::Unauthorized) => tracing::debug!("tidy-up lab: OpenAI refused the socket's token"),
        }
    });
}

async fn open(state: &AppState, token: &str) -> Result<Socket, Open> {
    let failed = |err: anyhow::Error| Open::Failed(err);
    let tls = state.tidy.https.tls("api.openai.com", 443).await.map_err(failed)?;
    let mut request = "wss://api.openai.com/v1/responses".into_client_request().map_err(|e| failed(e.into()))?;
    let auth = format!("Bearer {token}").parse().map_err(|_| failed(anyhow!("the access token isn't a header")))?;
    request.headers_mut().insert(header::AUTHORIZATION, auth);
    match tokio::time::timeout(REQUEST_TIMEOUT, tokio_tungstenite::client_async(request, tls)).await {
        Err(_) => Err(failed(anyhow!("OpenAI didn't open a socket within {REQUEST_TIMEOUT:?}"))),
        Ok(Ok((ws, _))) => Ok(Socket { ws, token: token.to_owned(), opened: Instant::now() }),
        Ok(Err(WsError::Http(response))) if response.status() == StatusCode::UNAUTHORIZED => Err(Open::Unauthorized),
        Ok(Err(WsError::Http(response))) => {
            let body = response.body().as_deref().unwrap_or_default();
            Err(failed(http_error(response.status(), body).into()))
        }
        Ok(Err(err)) => Err(failed(anyhow!("cannot open a socket to OpenAI: {err}"))),
    }
}

async fn turn(socket: &mut Socket, event: &str) -> Result<Answer, Exchange> {
    let start = Instant::now();
    let broken = |err: anyhow::Error, heard: bool| Exchange::Broken { err, retry: !heard };
    socket.ws.send(Message::text(event)).await.map_err(|e| broken(anyhow!("sending to OpenAI failed: {e}"), false))?;
    let mut text = String::new();
    let mut first_token = None;
    let mut heard = false;
    while let Some(message) = socket.ws.next().await {
        let data = match message.map_err(|e| broken(anyhow!("OpenAI's socket broke: {e}"), heard))? {
            Message::Text(data) => data,
            Message::Close(frame) => return Err(broken(anyhow!("OpenAI closed the socket: {frame:?}"), heard)),
            _ => continue,
        };
        heard = true;
        let event: Value = serde_json::from_str(&data).map_err(|e| Exchange::Broken {
            err: anyhow!("OpenAI sent an event that didn't parse: {e}"),
            retry: false,
        })?;
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                first_token.get_or_insert_with(|| start.elapsed());
                text.push_str(event["delta"].as_str().unwrap_or_default());
            }
            "response.completed" => {
                let response = &event["response"];
                if text.is_empty() {
                    text = output_text(response);
                }
                let usage = &response["usage"];
                let count = |v: &Value| v.as_u64().unwrap_or_default();
                return Ok(Answer {
                    text: text.trim().to_owned(),
                    first_token,
                    total: start.elapsed(),
                    connect: None,
                    input_tokens: count(&usage["input_tokens"]),
                    output_tokens: count(&usage["output_tokens"]),
                    reasoning_tokens: count(&usage["output_tokens_details"]["reasoning_tokens"]),
                });
            }
            "response.failed" => return Err(Exchange::Failed(api_error(&event["response"]["error"]).into())),
            "response.incomplete" => {
                let reason = &event["response"]["incomplete_details"]["reason"];
                return Err(Exchange::Failed(anyhow!("The response is incomplete: {reason}")));
            }
            "error" => return Err(Exchange::Failed(api_error(event.get("error").unwrap_or(&event)).into())),
            _ => {}
        }
    }
    Err(broken(anyhow!("OpenAI closed the socket"), heard))
}

fn output_text(response: &Value) -> String {
    let items = response["output"].as_array().into_iter().flatten();
    let parts = items.flat_map(|item| item["content"].as_array().into_iter().flatten());
    parts.filter_map(|part| part["text"].as_str()).collect()
}

async fn call(state: &AppState, method: Method, url: &str, token: &str, body: Bytes) -> Result<(StatusCode, Bytes)> {
    let headers = [(header::AUTHORIZATION, format!("Bearer {token}"))];
    exchange(state, method, url, &headers, body).await
}

async fn post_form(state: &AppState, url: &str, fields: &[(&str, &str)]) -> Result<(StatusCode, Bytes)> {
    let headers = [
        (header::CONTENT_TYPE, "application/x-www-form-urlencoded".to_owned()),
        (header::ACCEPT, "application/json".to_owned()),
    ];
    exchange(state, Method::POST, url, &headers, Bytes::from(encode_form(fields))).await
}

async fn exchange(
    state: &AppState,
    method: Method,
    url: &str,
    headers: &[(HeaderName, String)],
    body: Bytes,
) -> Result<(StatusCode, Bytes)> {
    let exchange = async {
        let response = state.tidy.https.send(method, url, headers, body).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        anyhow::Ok((status, body))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, exchange).await.map_err(|_| anyhow!("{url} didn't answer in time"))?
}

fn token_response(status: StatusCode, body: &[u8]) -> Result<TokenResponse> {
    if status.is_success() {
        return serde_json::from_slice(body).context("OpenAI's token response didn't parse");
    }
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = json["error"]["code"].as_str().or(json["error"].as_str());
    match code {
        Some(code) => {
            let message = json["error_description"].as_str().or(json["error"]["message"].as_str()).unwrap_or_default();
            Err(ApiError { code: code.to_owned(), message: message.to_owned() }.into())
        }
        None => bail!("OpenAI's token endpoint answered {status}: {}", String::from_utf8_lossy(body).trim()),
    }
}

fn http_error(status: StatusCode, body: &[u8]) -> ApiError {
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if json["error"].is_object() {
        return api_error(&json["error"]);
    }
    let message =
        json["detail"].as_str().map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
    ApiError { code: format!("HTTP {}", status.as_u16()), message }
}

fn api_error(error: &Value) -> ApiError {
    let code = error["code"].as_str().unwrap_or("error").to_owned();
    let mut message = error["message"].as_str().unwrap_or_default().to_owned();
    if let Some(param) = error["param"].as_str() {
        message = format!("{message} (param: {param})");
    }
    ApiError { code, message }
}

fn host(state: &AppState) -> Result<Host> {
    if let Some(host) = state.db.get_state(HOST_KEY)?.and_then(|text| serde_json::from_str(&text).ok()) {
        return Ok(host);
    }
    let host = Host { ext_agent_host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()), client_id: None };
    save(state, HOST_KEY, &host)?;
    Ok(host)
}

fn save(state: &AppState, key: &str, value: &impl Serialize) -> Result<()> {
    state.db.set_state(key, &serde_json::to_string(value)?)
}

fn jwt_claims(jwt: &str) -> Result<Value> {
    let payload = jwt.split('.').nth(1).ok_or_else(|| anyhow!("the ID token isn't a JWT"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).context("the ID token isn't base64")?;
    serde_json::from_slice(&bytes).context("the ID token's claims didn't parse")
}

fn scopes(scope: Option<&str>) -> Vec<String> {
    scope.unwrap_or_default().split_whitespace().map(str::to_owned).collect()
}

fn encode_form(fields: &[(&str, &str)]) -> String {
    let pairs: Vec<String> =
        fields.iter().map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, QUERY_VALUE))).collect();
    pairs.join("&")
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let decode = |s: &str| percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned();
    query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect()
}

fn random_token(bytes: usize) -> String {
    let random: [u8; 32] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(&random[..bytes.min(32)])
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{ServeConfig, serve_in_process};

    #[tokio::test]
    async fn cancelling_while_openai_exchanges_the_code_leaves_chatgpt_disconnected() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(config_dir.join("server.toml"), "listen = []\n[model]\npreload = false\n").unwrap();
        let server = serve_in_process(ServeConfig {
            config_dir,
            state_dir: dir.path().join("state"),
            cache_dir: dir.path().join("cache"),
            socket: Some(dir.path().join("psst.sock")),
        })
        .await
        .unwrap();
        let state = server.state.clone();

        start(&state, None).await.unwrap();
        let url = state.tidy.sign_in().pending.as_ref().unwrap().url.clone();
        let authorize = parse_query(url.split_once('?').unwrap().1);
        let (csrf, nonce) = (authorize["state"].clone(), authorize["nonce"].clone());

        let (client, mut openai) = tokio::io::duplex(64 * 1024);
        state.tidy.https.connect_to("auth.openai.com:443", client).await;
        let user = state.clone();
        let exchange = tokio::spawn(async move {
            assert!(openai.read(&mut [0; 4096]).await.unwrap() > 0, "the token request reaches OpenAI");
            cancel(&user);
            let claims = json!({ "nonce": nonce, "aud": "app", "email": "me@example.com", "sub": "me" });
            let body = json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "id_token": format!("header.{}.signature", URL_SAFE_NO_PAD.encode(claims.to_string())),
                "scope": SCOPE,
            })
            .to_string();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            openai.write_all(response.as_bytes()).await.unwrap();
        });

        let callback = format!("http://127.0.0.1:{PORT}/auth/callback?code=code&state={csrf}&client_id=app");
        let err = paste(&state, &callback).await.unwrap_err();
        assert_eq!(err.to_string(), "The sign-in was cancelled.");
        assert!(credentials(&state).is_none());
        exchange.await.unwrap();
        server.shutdown().await.unwrap();
    }
}
