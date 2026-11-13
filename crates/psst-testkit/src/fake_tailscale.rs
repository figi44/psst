//! tailscaled's LocalAPI as far as `auto_pair` asks it: `whois` and `status`, on a Unix socket.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{IpAddr, SocketAddr},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use serde_json::json;
use tempfile::TempDir;

/// The fake's own tailnet.
pub const TAILNET: &str = "tail1234.ts.net";

type Machines = Arc<Mutex<HashMap<IpAddr, (String, String)>>>;

#[derive(Debug)]
pub struct FakeTailscale {
    socket: PathBuf,
    machines: Machines,
    stop: Arc<AtomicBool>,
    _dir: TempDir,
}

impl FakeTailscale {
    pub fn start() -> FakeTailscale {
        let dir = tempfile::Builder::new().prefix("psst-tailscaled-").tempdir().expect("temp dir");
        let socket = dir.path().join("tailscaled.sock");
        let listener = UnixListener::bind(&socket).expect("bind the fake tailscaled socket");
        let machines = Machines::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (served, stopped) = (machines.clone(), stop.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = stream {
                    answer(stream, &served);
                }
            }
        });
        FakeTailscale { socket, machines, stop, _dir: dir }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// `name` is the full MagicDNS name: a machine shared in from another tailnet has that
    /// tailnet's suffix.
    pub fn add(&self, ip: IpAddr, stable_id: &str, name: &str) {
        self.machines.lock().unwrap().insert(ip, (stable_id.into(), name.into()));
    }

    pub fn remove(&self, ip: IpAddr) {
        self.machines.lock().unwrap().remove(&ip);
    }
}

impl Drop for FakeTailscale {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
    }
}

fn answer(stream: UnixStream, machines: &Machines) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut host = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("host")
        {
            host = Some(value.trim().to_owned());
        }
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let (status, body) = if host.as_deref() != Some("local-tailscaled.sock") {
        // Like tailscaled, which guards against DNS rebinding.
        (403, json!("invalid localapi request"))
    } else if target == "/localapi/v0/status?peers=false" {
        (
            200,
            json!({
                "Self": {"DNSName": format!("server.{TAILNET}.")},
                "CurrentTailnet": {"Name": "alice@example.com", "MagicDNSSuffix": TAILNET, "MagicDNSEnabled": true},
            }),
        )
    } else if let Some(addr) = target.strip_prefix("/localapi/v0/whois?addr=") {
        let machine = decode(addr).parse::<SocketAddr>().ok().and_then(|addr| {
            let ip = addr.ip().to_canonical();
            machines.lock().unwrap().get(&ip).cloned().map(|machine| (ip, machine))
        });
        match machine {
            Some((ip, (id, name))) => (
                200,
                json!({
                    "Node": {"StableID": id, "Name": format!("{name}."), "Addresses": [format!("{ip}/32")]},
                    "UserProfile": {"LoginName": "alice@example.com"},
                }),
            ),
            None => (404, json!("no match for IP:port")),
        }
    } else {
        (404, json!("not found"))
    };
    let body = body.to_string();
    let _ = write!(
        &stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn decode(text: &str) -> String {
    let mut out = Vec::new();
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        let hex = |b: Option<u8>| b.and_then(|b| (b as char).to_digit(16));
        match b {
            b'%' => match (hex(bytes.next()), hex(bytes.next())) {
                (Some(hi), Some(lo)) => out.push((hi * 16 + lo) as u8),
                _ => return String::new(),
            },
            _ => out.push(b),
        }
    }
    String::from_utf8(out).unwrap_or_default()
}
