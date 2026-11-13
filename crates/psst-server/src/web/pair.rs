//! Pairing a phone by QR code: the Devices page's form shows one, and the page a phone opens from
//! it, or from `psst-server pair`'s, hands the code to the app. Only the app redeems the code, so
//! opening the page, or a link preview fetching it, uses nothing up.

use std::net::IpAddr;

use axum::{
    Form,
    extract::{
        Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::HOST},
    response::Response,
};
use minijinja::{Value, context};
use psst_proto::{
    Endpoint,
    api::{encode_segment, routes},
};
use qrcodegen::{QrCode, QrCodeEcc};
use serde::Deserialize;

use super::{AppRef, Viewer, chrome, message, render};
use crate::{api, auth};

/// Where Chrome sends a phone without the app.
const RELEASES: &str = "https://github.com/figi44/psst/releases";

#[derive(Debug, Deserialize)]
pub struct PairQuery {
    code: Option<String>,
}

pub async fn page(query: Result<Query<PairQuery>, QueryRejection>, headers: HeaderMap) -> Response {
    let code = query.ok().and_then(|Query(query)| query.code).map(|code| code.trim().to_owned());
    let Some(code) = code.filter(|code| !code.is_empty()) else {
        return message(
            StatusCode::OK,
            "Pair a phone",
            "Pairing takes a code. Run this on the server's machine, then scan its QR code with the phone:",
            Some("psst-server pair --name phone"),
        );
    };
    let Some(server) = reached_as(&headers) else {
        return message(
            StatusCode::BAD_REQUEST,
            "Not a server address",
            "This page was opened by an address psst can't use; open the link psst-server pair printed.",
            None,
        );
    };
    let app = format!(
        "intent://pair?server={}&code={}#Intent;scheme=psst;package=io.github.figi44.psst;S.browser_fallback_url={};end",
        encode_segment(&server.to_string()),
        encode_segment(&code),
        encode_segment(RELEASES)
    );
    render(StatusCode::OK, "pair.html", context! { server => server.to_string(), code, app })
}

#[derive(Debug, Deserialize)]
pub struct NewPairing {
    name: String,
}

/// Only for this machine's users: a paired device that could make codes could pair itself again
/// under another name and keep its access after being unpaired.
pub async fn create(
    viewer: Viewer,
    State(state): State<AppRef>,
    headers: HeaderMap,
    form: Result<Form<NewPairing>, FormRejection>,
) -> Response {
    if !viewer.local {
        return message(
            StatusCode::FORBIDDEN,
            "Pair from the server's machine",
            "This browser logged in through a paired device, and paired devices can't pair others: unpairing \
             one would leave the devices it paired. Run this on the server's machine instead:",
            Some("psst-server pair --name phone"),
        );
    }
    let name = form.ok().map(|Form(form)| form.name.trim().to_owned()).filter(|name| !name.is_empty());
    let name = name.unwrap_or_else(|| "phone".to_owned());
    let pairing = state.pairings.create(name.clone());
    tracing::info!("pairing code for {name} created in the web UI; valid until {}", pairing.expires_at);
    // A browser on the server's own machine reached it by an address no phone can use.
    let reached = reached_as(&headers).filter(|server| !on_this_machine(server));
    let server = api::own_address(&state, reached).await;
    let link = server.as_ref().map(|server| format!("{server}{}?code={}", routes::PAIR_PAGE, pairing.code));
    let ctx = context! {
        name,
        code => pairing.code,
        minutes => auth::CODE_TTL.as_secs() / 60,
        server => server.map(|server| server.to_string()),
        qr => link.as_deref().map(qr_svg).map(Value::from_safe_string),
        link,
        ..chrome(&state, &viewer, "devices")
    };
    render(StatusCode::OK, "pairing.html", ctx)
}

fn on_this_machine(server: &Endpoint) -> bool {
    let Endpoint::Tcp { host, .. } = server else { return true };
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Black on white in either theme, with the light border scanners need.
fn qr_svg(text: &str) -> String {
    const BORDER: i32 = 4;
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let size = qr.size() + 2 * BORDER;
    let mut modules = String::new();
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                modules.push_str(&format!("M{},{}h1v1h-1z", x + BORDER, y + BORDER));
            }
        }
    }
    format!(
        r##"<svg class="qr" viewBox="0 0 {size} {size}" shape-rendering="crispEdges" role="img" aria-label="QR code of the pairing link"><rect width="{size}" height="{size}" fill="#fff"/><path fill="#000" d="{modules}"/></svg>"##
    )
}

/// The address the phone just reached this server by, so one the app can reach it by too.
fn reached_as(headers: &HeaderMap) -> Option<Endpoint> {
    let host = headers.get(HOST)?.to_str().ok()?;
    // Browsers leave out port 80, which the app would take for psst's own default port.
    let has_port =
        host.rsplit_once(':').is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let authority = if has_port { host.to_owned() } else { format!("{host}:80") };
    format!("http://{authority}").parse().ok()
}
