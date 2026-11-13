//! Logging a browser in with a one-time link from `psst ui`, and out.
//!
//! `GET /login?token=` only shows a form; posting it to `/api/login/redeem` uses the token up and
//! sets the session cookie. A link preview that fetches the page changes nothing.

use std::{sync::Arc, time::Instant};

use axum::{
    Form,
    extract::{
        ConnectInfo, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::{
        HeaderMap, StatusCode,
        header::{LOCATION, SET_COOKIE},
    },
    response::{IntoResponse, Response},
};
use minijinja::context;
use serde::Deserialize;

use super::{Viewer, cross_site, message, render};
use crate::{
    api,
    auth::{self, RedeemError, SESSION_COOKIE, SESSION_TTL},
    listen::Peer,
    state::AppState,
};

type AppRef = Arc<AppState>;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RedeemForm {
    token: String,
}

/// `GET /login?token=`: the form that logs in with the token.
pub async fn page(query: Result<Query<LoginQuery>, QueryRejection>) -> Response {
    match query.ok().and_then(|Query(query)| query.token).filter(|token| !token.is_empty()) {
        Some(token) => render(StatusCode::OK, "login.html", context! { token }),
        None => message(
            StatusCode::OK,
            "Log in to psst",
            "The web UI opens with a one-time link. Run this on the server's machine, or on a device paired \
             with it:",
            Some("psst ui"),
        ),
    }
}

/// `POST /api/login/redeem`: uses up the link's token and starts a session for the device that
/// asked for the link.
pub async fn redeem(
    State(state): State<AppRef>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    form: Result<Form<RedeemForm>, FormRejection>,
) -> Response {
    let Peer::Tcp(addr) = peer else {
        return message(
            StatusCode::NOT_FOUND,
            "Not here",
            "Browsers log in over TCP; open the link psst ui gives you.",
            None,
        );
    };
    if !api::same_origin(&state, &headers) {
        return cross_site();
    }
    let Ok(Form(form)) = form else {
        return message(StatusCode::BAD_REQUEST, "No login token", "Open the link psst ui gives you.", Some("psst ui"));
    };
    let device = match state.logins.redeem(addr.ip(), &form.token, Instant::now()) {
        Ok(device) => device,
        Err(RedeemError::RateLimited) => {
            return message(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts",
                "Too many login attempts in the last minute. Wait a minute, then get a new link:",
                Some("psst ui"),
            );
        }
        Err(RedeemError::Invalid) => {
            return message(
                StatusCode::FORBIDDEN,
                "This login link doesn't work",
                "It's unknown, already used, or more than a minute old. Get a new one:",
                Some("psst ui"),
            );
        }
    };
    if !api::device_allowed(&state, &device) {
        return message(
            StatusCode::FORBIDDEN,
            "Not allowed",
            "The device that asked for this link isn't allowed any more.",
            None,
        );
    }
    let secret = auth::new_secret();
    let expires = jiff::Timestamp::now() + SESSION_TTL;
    if let Err(err) = state.db.add_web_session(&auth::hash_token(&secret), &device, expires) {
        tracing::error!("cannot start a browser session: {err:#}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error; see the server's log").into_response();
    }
    tracing::info!("browser session for {device} started from {addr}; valid until {expires}");
    let cookie = session_cookie(&state, &secret, SESSION_TTL.as_secs());
    (StatusCode::SEE_OTHER, [(LOCATION, "/dictionary".to_owned()), (SET_COOKIE, cookie)]).into_response()
}

/// `POST /logout`: ends the browser's session.
pub async fn logout(viewer: Viewer, State(state): State<AppRef>) -> Response {
    if let Some(session) = &viewer.session
        && let Err(err) = state.db.delete_web_session(session)
    {
        tracing::error!("cannot end a browser session: {err:#}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error; see the server's log").into_response();
    }
    let mut page = message(StatusCode::OK, "Logged out", "To log in again, run:", Some("psst ui"));
    if let Ok(cookie) = session_cookie(&state, "", 0).parse() {
        page.headers_mut().insert(SET_COOKIE, cookie);
    }
    page
}

/// The session cookie: only for this server's own requests, never for scripts.
fn session_cookie(state: &AppState, value: &str, max_age_s: u64) -> String {
    let secure = state.server.web_url.as_deref().is_some_and(|url| url.starts_with("https://"));
    format!(
        "{SESSION_COOKIE}={value}; Max-Age={max_age_s}; Path=/; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}
