//! Every page also works without JavaScript: htmx only swaps it in place (`hx-boost`).

mod dictionary;
mod history;
mod login;
mod pair;
mod profiles;
mod settings;
mod tidy;

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

const CSP: &str = "default-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";

const TEMPLATES: &[(&str, &str)] = &[
    ("icons.html", include_str!("../../../../web/templates/icons.html")),
    ("base.html", include_str!("../../../../web/templates/base.html")),
    ("message.html", include_str!("../../../../web/templates/message.html")),
    ("login.html", include_str!("../../../../web/templates/login.html")),
    ("pair.html", include_str!("../../../../web/templates/pair.html")),
    ("dictionary.html", include_str!("../../../../web/templates/dictionary.html")),
    ("profiles.html", include_str!("../../../../web/templates/profiles.html")),
    ("history.html", include_str!("../../../../web/templates/history.html")),
    ("post-processing.html", include_str!("../../../../web/templates/post-processing.html")),
    ("rows.html", include_str!("../../../../web/templates/rows.html")),
    ("settings.html", include_str!("../../../../web/templates/settings.html")),
    ("devices.html", include_str!("../../../../web/templates/devices.html")),
    ("device.html", include_str!("../../../../web/templates/device.html")),
];

const ASSETS: &[(&str, &str, &[u8])] = &[
    ("psst.css", "text/css; charset=utf-8", include_bytes!("../../../../web/static/psst.css")),
    ("login.js", "text/javascript; charset=utf-8", include_bytes!("../../../../web/static/login.js")),
    ("psst.js", "text/javascript; charset=utf-8", include_bytes!("../../../../web/static/psst.js")),
    ("htmx.min.js", "text/javascript; charset=utf-8", include_bytes!("../../../../web/vendor/htmx/htmx.min.js")),
    // The overlay's typeface, cut to Latin and Latin Extended-A. psst.css names it, so its file
    // name carries the version instead of `?v=`.
    ("inter-4.001-medium.woff2", "font/woff2", include_bytes!("../../../../web/vendor/inter/inter-4.001-medium.woff2")),
];

pub fn router() -> Router<AppRef> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/history") }))
        .route("/history", get(history::show))
        .route("/history/{id}/fix", post(history::fix))
        .route("/history/{id}/undo", post(history::undo))
        .route("/login", get(login::page))
        .route(routes::LOGIN_REDEEM, post(login::redeem))
        .route("/logout", post(login::logout))
        .route(routes::PAIR_PAGE, get(pair::page))
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
        .route("/post-processing", get(tidy::show).post(tidy::act))
        .route("/post-processing/styles", post(tidy::create_style))
        .route("/post-processing/styles/{id}", post(tidy::save_style))
        .route("/post-processing/connect", post(tidy::connect))
        .route("/post-processing/connect/paste", post(tidy::paste))
        .route("/post-processing/connect/cancel", post(tidy::cancel))
        .route("/post-processing/disconnect", post(tidy::disconnect))
        .route("/post-processing/runs/clear", post(tidy::clear))
        .route("/post-processing/eval", post(tidy::eval))
        .route("/post-processing/eval/models", get(tidy::eval_models))
        .route("/settings", get(settings::show))
        .route("/settings.toml", get(settings::export))
        .route("/settings/{key}", post(settings::set))
        .route("/settings/{key}/reset", post(settings::reset))
        .route("/devices", get(settings::devices))
        .route("/devices/{id}", get(settings::device))
        .route("/devices/{id}/client.toml", get(settings::export_device))
        .route("/devices/{id}/setting/{key}", post(settings::set_device))
        .route("/devices/{id}/setting/{key}/reset", post(settings::reset_device))
        .route("/devices/{id}/app", post(settings::set_app))
        .route("/devices/{id}/app/{app}/remove", post(settings::remove_app))
        .route("/devices/{id}/app/{app}/reset", post(settings::reset_app))
        .route("/static/{file}", get(asset))
}

fn templates() -> &'static Environment<'static> {
    static TEMPLATES_ENV: OnceLock<Environment<'static>> = OnceLock::new();
    TEMPLATES_ENV.get_or_init(|| {
        let mut env = Environment::new();
        env.add_filter("segment", |value: String| psst_proto::api::encode_segment(&value));
        for (name, source) in TEMPLATES {
            env.add_template(name, source).expect("the web UI's templates parse");
        }
        env
    })
}

/// Goes in the assets' URLs, so browsers may cache them for good.
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

fn render(status: StatusCode, template: &str, ctx: Value) -> Response {
    let ctx = context! { assets => assets_version(), ..ctx };
    match templates().get_template(template).and_then(|template| template.render(ctx)) {
        Ok(html) => (
            status,
            [
                (CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP)),
                (X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
                (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
                // Not `no-referrer`: with it, browsers send `Origin: null` with a page's own
                // (non-script) form posts, which then fail the same-origin check.
                (REFERRER_POLICY, HeaderValue::from_static("same-origin")),
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

fn chrome(state: &AppState, viewer: &Viewer, page: &str) -> Value {
    use psst_proto::api::ModelState;
    let model = state.engine.status();
    let (label, tone) = match model.state {
        ModelState::Loaded => ("Ready", ""),
        ModelState::Downloading => ("Downloading", "busy"),
        ModelState::Loading => ("Loading", "busy"),
        ModelState::Unloaded => ("Unloaded", "warn"),
        ModelState::Missing => ("Not downloaded", "warn"),
        ModelState::Failed => ("Failed", "bad"),
    };
    context! {
        page,
        signed_in => true,
        browser => viewer.session.is_some(),
        model => context! {
            name => model.name.clone(),
            state => label,
            tone,
            text => crate::admin::describe_model(&model),
        },
        banner => banner(&model),
    }
}

fn banner(model: &psst_proto::api::ModelStatus) -> Option<Value> {
    use psst_proto::api::ModelState;
    let load = Some("psst-server model load");
    let (tone, title, text, command) = match model.state {
        ModelState::Loaded => {
            let reload = model.reload.as_ref()?;
            let waiting = match reload.waiting_for {
                1 => "the dictation in progress finishes".to_owned(),
                n => format!("the {n} dictations in progress finish"),
            };
            ("", format!("Switching to {}", reload.model), format!("once {waiting}."), None)
        }
        ModelState::Downloading => (
            "",
            "Downloading the speech model.".to_owned(),
            match model.progress.and_then(|p| p.total.map(|total| (p.bytes, total))) {
                Some((bytes, total)) => {
                    format!("{} of {} MB. Dictations wait until it's ready.", bytes / 1_000_000, total / 1_000_000)
                }
                None => "Dictations wait until it's ready.".to_owned(),
            },
            None,
        ),
        ModelState::Loading => {
            ("", "Loading the speech model.".to_owned(), "Dictations wait until it's ready.".to_owned(), None)
        }
        ModelState::Unloaded => {
            ("warn", "The speech model is unloaded,".to_owned(), "so dictations fail.".to_owned(), load)
        }
        ModelState::Missing => {
            ("warn", "The speech model isn't downloaded,".to_owned(), "so dictations fail.".to_owned(), load)
        }
        ModelState::Failed => (
            "bad",
            "The speech model failed to load:".to_owned(),
            format!(
                "{}. Dictations fail until it loads; fix the cause, then load it again.",
                model.error.as_deref().unwrap_or("unknown error")
            ),
            load,
        ),
    };
    let progress = model.progress.and_then(|p| p.total.map(|total| context! { bytes => p.bytes, total }));
    Some(context! { tone, title, text, command, progress })
}

pub(crate) fn message(status: StatusCode, title: &str, text: &str, command: Option<&str>) -> Response {
    render(status, "message.html", context! { title, text, command })
}

fn log_in_first() -> Response {
    message(
        StatusCode::UNAUTHORIZED,
        "Log in to psst",
        "This browser isn't logged in, or its session has ended. Log in with a one-time link: run this on \
         the server's machine, or on a device paired with it.",
        Some("psst ui"),
    )
}

pub(crate) struct Viewer {
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
