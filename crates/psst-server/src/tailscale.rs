//! tailscaled's LocalAPI, which says which tailnet machine a peer address belongs to, and what
//! this machine is called on the tailnet.

use std::{net::SocketAddr, path::Path, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, de::DeserializeOwned};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// Tailscale's StableID, which survives renames.
    pub id: String,
    /// The MagicDNS short name.
    pub name: String,
}

#[derive(Deserialize)]
struct WhoIs {
    #[serde(rename = "Node")]
    node: Node,
}

#[derive(Deserialize)]
struct Node {
    #[serde(rename = "StableID")]
    stable_id: String,
    /// The MagicDNS name, with a trailing dot.
    #[serde(rename = "Name")]
    name: String,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Self")]
    this: Option<SelfNode>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<Tailnet>,
}

#[derive(Deserialize)]
struct SelfNode {
    /// With a trailing dot; empty while logged out.
    #[serde(rename = "DNSName", default)]
    dns_name: String,
}

#[derive(Deserialize)]
struct Tailnet {
    #[serde(rename = "MagicDNSSuffix")]
    magic_dns_suffix: String,
}

/// `None` for an address that isn't a machine on this server's own tailnet. A machine shared in
/// from another tailnet has that tailnet's MagicDNS suffix, so it's `None` too.
pub async fn whois(socket: &Path, addr: SocketAddr) -> Result<Option<Machine>> {
    let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
    let Some(who) = get::<WhoIs>(socket, &format!("/localapi/v0/whois?addr={}", encode(&addr.to_string()))).await?
    else {
        return Ok(None);
    };
    let status = get::<Status>(socket, "/localapi/v0/status?peers=false")
        .await?
        .ok_or_else(|| anyhow!("tailscaled has no status"))?;
    let suffix =
        status.current_tailnet.ok_or_else(|| anyhow!("tailscaled isn't logged in to a tailnet"))?.magic_dns_suffix;
    let name = who.node.name.strip_suffix('.').unwrap_or(&who.node.name);
    let short = name.strip_suffix(suffix.as_str()).and_then(|name| name.strip_suffix('.'));
    Ok(short
        .filter(|short| !short.is_empty() && !short.contains('.'))
        .map(|short| Machine { id: who.node.stable_id, name: short.to_owned() }))
}

/// This machine's full MagicDNS name. `None` while tailscaled isn't logged in to a tailnet.
pub async fn own_name(socket: &Path) -> Result<Option<String>> {
    let status = get::<Status>(socket, "/localapi/v0/status?peers=false").await?;
    let name = status.and_then(|status| status.this).map(|node| node.dns_name);
    Ok(name.map(|name| name.trim_end_matches('.').to_owned()).filter(|name| !name.is_empty()))
}

/// `None` for a 404.
async fn get<T: DeserializeOwned>(socket: &Path, path: &str) -> Result<Option<T>> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("cannot connect to {}", socket.display()))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let request = Request::get(path).header(header::HOST, "local-tailscaled.sock").body(Empty::<Bytes>::new())?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        anyhow::Ok((status, body))
    };
    let (status, body) =
        tokio::time::timeout(TIMEOUT, exchange).await.map_err(|_| anyhow!("no answer within {TIMEOUT:?}"))??;
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("{path}: {status}: {}", String::from_utf8_lossy(&body).trim());
    }
    serde_json::from_slice(&body).map(Some).with_context(|| format!("{path}: unexpected answer"))
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
