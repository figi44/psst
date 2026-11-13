//! The web UI (plan 6.8) and the browser login (plan 6.6): server-rendered HTML from the
//! templates in `web/templates`, with the stylesheet and scripts in `web/static`, all embedded in
//! the binary.
//!
//! Every page is rendered whole, with minijinja escaping every value; a page that uses htmx swaps
//! itself in place (`hx-boost`), so each one also works without JavaScript. Forms post and
//! redirect back with `303`; a change the checks refuse renders the page again with status 422,
//! the reason and the values entered.

mod dictionary;
mod login;
mod profiles;

use std::sync::{Arc, OnceLock};

use axum::{
    Router,
    extract::{FromRequestParts, Path},
    http::{
        HeaderValue, StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
            X_FRAME_OPTIONS,
        },
        request::Parts,
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use minijinja::{Environment, Value, context};
use psst_proto::api::routes;
use sha2::{Digest, Sha256};

use crate::{
    api::{self, Caller},
    auth::Refusal,
    state::AppState,
};

type AppRef = Arc<AppState>;

/// The pages' policy: nothing from elsewhere, no inline scripts or styles, no framing.
const CSP: &str = "default-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";

const TEMPLATES: &[(&str, &str)] = &[
    ("base.html", include_str!("../../../../web/templates/base.html")),
    ("message.html", include_str!("../../../../web/templates/message.html")),
    ("login.html", include_str!("../../../../web/templates/login.html")),
    ("dictionary.html", include_str!("../../../../web/templates/dictionary.html")),
    ("profiles.html", include_str!("../../../../web/templates/profiles.html")),
];

/// Static files: name, content type, contents.
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("psst.css", "text/css; charset=utf-8", include_bytes!("../../../../web/static/psst.css")),
    ("login.js", "text/javascript; charset=utf-8", include_bytes!("../../../../web/static/login.js")),
    ("htmx.min.js", "text/javascript; charset=utf-8", include_bytes!("../../../../web/vendor/htmx/htmx.min.js")),
];

/// The web UI's routes; the API's are in [`crate::api`].
pub fn router() -> Router<AppRef> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/dictionary") }))
        .route("/login", get(login::page))
        .route(routes::LOGIN_REDEEM, post(login::redeem))
        .route("/logout", post(login::logout))
        .route("/dictionary", get(dictionary::show))
        .route("/dictionary.toml", get(dictionary::export))
        .route("/dictionary/terms", post(dictionary::add))
        .route("/dictionary/terms/{id}", post(dictionary::save))
        .route("/dictionary/terms/{id}/hide", post(dictionary::hide))
        .route("/dictionary/terms/{id}/reset", post(dictionary::reset))
        .route("/dictionary/never", post(dictionary::add_never))
        .route("/dictionary/never/{id}/remove", post(dictionary::remove_never))
        .route("/profiles", get(profiles::show).post(profiles::add))
        .route("/profiles.toml", get(profiles::export))
        .route("/profiles/{name}", post(profiles::save))
        .route("/profiles/{name}/reset", post(profiles::reset))
        .route("/static/{file}", get(asset))
}

fn templates() -> &'static Environment<'static> {
    static TEMPLATES_ENV: OnceLock<Environment<'static>> = OnceLock::new();
    TEMPLATES_ENV.get_or_init(|| {
        let mut env = Environment::new();
        // A value as one path segment (or query value): everything but unreserved characters
        // percent-encoded, `/` included.
        env.add_filter("segment", |value: String| psst_proto::api::encode_segment(&value));
        for (name, source) in TEMPLATES {
            env.add_template(name, source).expect("the web UI's templates parse");
        }
        env
    })
}

/// A short hash of every asset, in their URLs, so browsers may cache them for good.
fn assets_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hash = Sha256::new();
        for (name, _, body) in ASSETS {
            hash.update(name.as_bytes());
            hash.update(body);
        }
        hash.finalize().iter().take(6).map(|byte| format!("{byte:02x}")).collect()
    })
}

async fn asset(Path(file): Path<String>) -> Response {
    match ASSETS.iter().find(|(name, ..)| *name == file) {
        Some((_, content_type, body)) => (
            [
                (CONTENT_TYPE, *content_type),
                (CACHE_CONTROL, "public, max-age=31536000, immutable"),
                (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            *body,
        )
            .into_response(),
        None => message(StatusCode::NOT_FOUND, "Not found", "There's no such file.", None),
    }
}

/// Renders a page with `ctx`, plus what every page needs (`assets`).
fn render(status: StatusCode, template: &str, ctx: Value) -> Response {
    let ctx = context! { assets => assets_version(), ..ctx };
    match templates().get_template(template).and_then(|template| template.render(ctx)) {
        Ok(html) => (
            status,
            [
                (CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP)),
                (X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
                (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
                (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
                (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            Html(html),
        )
            .into_response(),
        Err(err) => {
            tracing::error!("cannot render {template}: {err:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error; see the server's log").into_response()
        }
    }
}

/// What a signed-in page's header needs: which page it is, the model's state, and whether there's
/// a browser session to log out of.
fn chrome(state: &AppState, viewer: &Viewer, page: &str) -> Value {
    let model = state.engine.status();
    let model_state = serde_json::to_value(model.state).ok().and_then(|v| v.as_str().map(String::from));
    context! {
        page,
        signed_in => true,
        browser => viewer.session.is_some(),
        model => crate::admin::describe_model(&model),
        model_state,
    }
}

/// A page with a title, a sentence and, optionally, a command to run.
pub(crate) fn message(status: StatusCode, title: &str, text: &str, command: Option<&str>) -> Response {
    render(status, "message.html", context! { title, text, command })
}

/// The page for a request without a live session.
fn log_in_first() -> Response {
    message(
        StatusCode::UNAUTHORIZED,
        "Log in to psst",
        "This browser isn't logged in, or its session has ended. Log in with a one-time link: run this on \
         the server's machine, or on a device paired with it.",
        Some("psst ui"),
    )
}

/// An authenticated web UI caller: a browser by its session, or a local user or paired device.
pub(crate) struct Viewer {
    /// The browser session's hash, which logging out ends.
    session: Option<String>,
}

impl FromRequestParts<AppRef> for Viewer {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppRef) -> Result<Self, Response> {
        match api::authenticate(state, parts) {
            Ok(Caller::Browser { session, .. }) => Ok(Viewer { session: Some(session) }),
            Ok(Caller::Identity(_)) => Ok(Viewer { session: None }),
            Err(Refusal::Unauthorized) => Err(log_in_first()),
            Err(Refusal::Forbidden) => {
                Err(message(StatusCode::FORBIDDEN, "Not allowed", "This user isn't in the server's local_users.", None))
            }
            Err(Refusal::CrossSite) => Err(cross_site()),
        }
    }
}

fn cross_site() -> Response {
    message(
        StatusCode::FORBIDDEN,
        "Cross-site request refused",
        "This request didn't come from psst's own pages, so it was refused before it did anything.",
        None,
    )
}
