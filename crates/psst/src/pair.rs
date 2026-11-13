//! `psst pair <url> <code>` and the device token it stores in
//! `$XDG_STATE_HOME/psst/credentials` (mode 0600).

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

use psst_proto::{
    DeviceId, Endpoint, Token,
    api::{ApiError, PairRedeem, PairRedeemed, routes},
};
use serde::{Deserialize, Serialize};

use crate::store::{read_json, write_json_atomic};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The credentials file: the token a server issued to this device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// The server that issued the token.
    pub server: Endpoint,
    pub device_id: DeviceId,
    pub token: Token,
}

impl Credentials {
    pub fn load(path: &Path) -> io::Result<Option<Credentials>> {
        match read_json(path) {
            Ok(creds) => Ok(Some(creds)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_json_atomic(path, self, 0o600)
    }
}

/// Redeems a pairing code at `server` and returns the credentials to store.
pub fn redeem(server: &Endpoint, code: &str, name: &str) -> Result<Credentials, String> {
    let Endpoint::Tcp { host, port } = server else {
        return Err("pairing is for other machines: give the server as http://host[:port]".into());
    };
    let body = serde_json::to_string(&PairRedeem { code: code.trim().into(), name: name.into() })
        .map_err(|e| e.to_string())?;
    let (status, reply) = post(host, *port, routes::PAIR_REDEEM, &body).map_err(|e| format!("{server}: {e}"))?;
    if !(200..300).contains(&status) {
        let message = serde_json::from_slice::<ApiError>(&reply)
            .map(|err| err.message)
            .unwrap_or_else(|_| String::from_utf8_lossy(&reply).into_owned());
        return Err(format!("the server refused the code (HTTP {status}): {message}"));
    }
    let redeemed: PairRedeemed =
        serde_json::from_slice(&reply).map_err(|e| format!("unexpected answer from {server}: {e}"))?;
    Ok(Credentials { server: server.clone(), device_id: redeemed.device_id, token: redeemed.token })
}

/// This machine's name, for the device list.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "psst".into())
}

/// A minimal HTTP/1.1 POST with a JSON body: returns the status and the body.
fn post(host: &str, port: u16, path: &str, body: &str) -> io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("malformed HTTP response: {what}"));
    let head_end = response.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| bad("no header end"))?;
    let head = String::from_utf8_lossy(&response[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| bad("no status"))?;
    let chunked = lines.any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    let body = &response[head_end + 4..];
    let body = if chunked { dechunk(body).ok_or_else(|| bad("bad chunked body"))? } else { body.to_vec() };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_bodies_are_joined() {
        assert_eq!(dechunk(b"4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n").unwrap(), b"{\"a\":1}");
        assert_eq!(dechunk(b"5\r\nab"), None);
    }
}
