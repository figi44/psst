//! Every admin command goes through `/api` over the socket; none opens the database.

use std::{io::IsTerminal, path::Path, process::ExitCode, time::Duration};

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use psst_proto::{
    Endpoint,
    api::{
        ApiError, Device, DeviceKind, DictionaryExport, HistoryRow, ModelState, ModelStatus, Pairing, PairingRequest,
        ProfilesExport, Stats, routes,
    },
};
use qrcodegen::{QrCode, QrCodeEcc};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, Clone)]
pub enum Action {
    ModelLoad,
    ModelUnload,
    ModelStatus,
    Pair { name: String, server: Option<Endpoint> },
    DevicesList,
    DevicesRevoke { id: String },
    Stats,
    History { limit: u32 },
    DictExport,
    ProfilesExport,
}

pub fn run(socket: &Path, json: bool, action: Action) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
    match runtime.block_on(execute(socket, json, action)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("psst-server: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(socket: &Path, json: bool, action: Action) -> Result<ExitCode> {
    let api = Api { socket };
    match action {
        Action::ModelStatus => {
            let status: ModelStatus = api.call(Method::GET, routes::MODEL, None::<()>).await?;
            print(json, &status, describe_model);
            Ok(ExitCode::SUCCESS)
        }
        Action::ModelUnload => {
            let status: ModelStatus = api.call(Method::POST, routes::MODEL_UNLOAD, None::<()>).await?;
            print(json, &status, describe_model);
            Ok(ExitCode::SUCCESS)
        }
        Action::ModelLoad => {
            let mut status: ModelStatus = api.call(Method::POST, routes::MODEL_LOAD, None::<()>).await?;
            let mut shown = String::new();
            while matches!(status.state, ModelState::Loading | ModelState::Downloading | ModelState::Missing) {
                let line = describe_model(&status);
                if !json && line != shown {
                    println!("{line}");
                    shown = line;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                status = api.call(Method::GET, routes::MODEL, None::<()>).await?;
            }
            print(json, &status, describe_model);
            Ok(if status.state == ModelState::Loaded { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Action::Pair { name, server } => {
            let mut pairing: Pairing = api.call(Method::POST, routes::PAIRING, Some(PairingRequest { name })).await?;
            if server.is_some() {
                pairing.server = server;
            }
            print(json, &pairing, describe_pairing);
            Ok(ExitCode::SUCCESS)
        }
        Action::DevicesList => {
            let devices: Vec<Device> = api.call(Method::GET, routes::DEVICES, None::<()>).await?;
            print(json, &devices, |devices| {
                let mut out = format!("{:<20} {:<16} {:<6} {:<26} LAST SEEN", "ID", "NAME", "KIND", "CREATED");
                for device in devices {
                    let kind = match device.kind {
                        DeviceKind::Local => "local",
                        DeviceKind::Token => "token",
                    };
                    let last_seen = device.last_seen.map(|t| t.to_string()).unwrap_or_else(|| "never".into());
                    out.push_str(&format!(
                        "\n{:<20} {:<16} {:<6} {:<26} {last_seen}",
                        device.id,
                        device.name,
                        kind,
                        device.created_at.to_string()
                    ));
                }
                out
            });
            Ok(ExitCode::SUCCESS)
        }
        Action::DevicesRevoke { id } => {
            api.call_empty(Method::DELETE, &format!("/api/devices/{}", encode_segment(&id))).await?;
            if !json {
                println!("revoked {id}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::DictExport => {
            let export: DictionaryExport = api.call(Method::GET, routes::DICTIONARY_EXPORT, None::<()>).await?;
            if json {
                print(json, &export, |_| String::new());
            } else {
                print!("{}", export.toml);
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::ProfilesExport => {
            let export: ProfilesExport = api.call(Method::GET, routes::PROFILES_EXPORT, None::<()>).await?;
            if json {
                print(json, &export, |_| String::new());
            } else {
                print!("{}", export.toml);
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Stats => {
            let stats: Stats = api.call(Method::GET, routes::STATS, None::<()>).await?;
            print(json, &stats, |stats| {
                let mut out =
                    format!("{} sessions\n{:<34} {:>6} {:>8} {:>8}", stats.sessions, "", "count", "p50", "p95");
                for (field, p) in &stats.timings {
                    out.push_str(&format!("\n{field:<34} {:>6} {:>8} {:>8}", p.count, p.p50, p.p95));
                }
                out
            });
            Ok(ExitCode::SUCCESS)
        }
        Action::History { limit } => {
            let rows: Vec<HistoryRow> =
                api.call(Method::GET, &format!("{}?limit={limit}", routes::HISTORY), None::<()>).await?;
            print(json, &rows, |rows| {
                rows.iter()
                    .map(|row| {
                        let text = match &row.r#final {
                            Some(fin) => format!("{:?}", fin.text),
                            None => "(no result)".into(),
                        };
                        format!("#{} {} {} {} {text}", row.id, row.created_at, row.device, row.profile)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub(crate) fn describe_model(status: &ModelStatus) -> String {
    let state = match status.state {
        ModelState::Missing => "missing".to_owned(),
        ModelState::Downloading => match status.progress {
            Some(p) => match p.total {
                Some(total) => format!("downloading ({} of {} MB)", p.bytes / 1_000_000, total / 1_000_000),
                None => format!("downloading ({} MB)", p.bytes / 1_000_000),
            },
            None => "downloading".into(),
        },
        ModelState::Loading => "loading".into(),
        ModelState::Loaded => match &status.revision {
            Some(revision) => format!("loaded (revision {revision})"),
            None => "loaded".into(),
        },
        ModelState::Unloaded => "unloaded".into(),
        ModelState::Failed => format!("failed: {}", status.error.as_deref().unwrap_or("unknown error")),
    };
    match &status.reload {
        Some(reload) => {
            let waiting = match reload.waiting_for {
                1 => "the dictation in progress finishes".to_owned(),
                n => format!("the {n} dictations in progress finish"),
            };
            format!("{}: {state}; switching to {} once {waiting}", status.name, reload.model)
        }
        None => format!("{}: {state}", status.name),
    }
}

fn describe_pairing(pairing: &Pairing) -> String {
    let code = &pairing.code;
    let mut out = format!("Pairing code: {code}\nValid until {} for one device.", pairing.expires_at);
    let Some(server) = &pairing.server else {
        out.push_str(&format!(
            "\nOn that device run: psst pair http://<this machine>:{} {code}",
            psst_proto::DEFAULT_PORT
        ));
        return out;
    };
    let link = format!("{server}{}?code={code}", routes::PAIR_PAGE);
    out.push_str("\n\nOn a phone, scan this with the camera, or open the link:\n");
    if std::io::stdout().is_terminal() {
        out.push_str(&qr_code(&link));
    }
    out.push_str(&format!("{link}\n\nOn a computer, run: psst pair {server} {code}"));
    out
}

/// Black on white whatever the terminal's colours, with the light border scanners need. A
/// half-block character holds two modules, one above the other, so that modules come out about
/// square.
fn qr_code(text: &str) -> String {
    const BORDER: i32 = 4;
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let mut out = String::new();
    for y in (-BORDER..qr.size() + BORDER).step_by(2) {
        out.push_str("\x1b[30;107m");
        for x in -BORDER..qr.size() + BORDER {
            // Light outside the code, which is the border.
            out.push(match (qr.get_module(x, y), qr.get_module(x, y + 1)) {
                (false, false) => ' ',
                (true, false) => '▀',
                (false, true) => '▄',
                (true, true) => '█',
            });
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn print<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).expect("API types serialise"));
    } else {
        println!("{}", human(value));
    }
}

fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct Api<'a> {
    socket: &'a Path,
}

impl Api<'_> {
    async fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<(StatusCode, Bytes)> {
        let stream = tokio::net::UnixStream::connect(self.socket)
            .await
            .with_context(|| format!("cannot connect to {} (is psst-server running?)", self.socket.display()))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let mut request = Request::builder().method(method).uri(path).header(header::HOST, "localhost");
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        if !status.is_success() {
            let message = match serde_json::from_slice::<ApiError>(&bytes) {
                Ok(err) => err.message,
                Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
            };
            return Err(anyhow!("{message} ({status})"));
        }
        Ok((status, bytes))
    }

    async fn call<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<impl Serialize>) -> Result<T> {
        let body = body.map(|body| serde_json::to_vec(&body).expect("API types serialise"));
        let (_, bytes) = self.request(method, path, body).await?;
        serde_json::from_slice(&bytes).context("unexpected answer from the server")
    }

    async fn call_empty(&self, method: Method, path: &str) -> Result<()> {
        self.request(method, path, None).await.map(drop)
    }
}
