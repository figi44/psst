//! The page a phone opens from `psst-server pair`'s link or QR code. Only the app redeems the
//! code, so opening the page, or a link preview fetching it, uses nothing up.

use axum::{
    extract::{Query, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header::HOST},
    response::Response,
};
use minijinja::context;
use psst_proto::{Endpoint, api::encode_segment};
use serde::Deserialize;

use super::{message, render};

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

/// The address the phone just reached this server by, so one the app can reach it by too.
fn reached_as(headers: &HeaderMap) -> Option<Endpoint> {
    let host = headers.get(HOST)?.to_str().ok()?;
    // Browsers leave out port 80, which the app would take for psst's own default port.
    let has_port =
        host.rsplit_once(':').is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let authority = if has_port { host.to_owned() } else { format!("{host}:80") };
    format!("http://{authority}").parse().ok()
}
