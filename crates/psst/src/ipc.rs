use std::{
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use jiff::Timestamp;
use psst_proto::{ClientTimings, SessionId};
use serde::{Deserialize, Serialize};

pub const CLI_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    Start,
    Stop,
    Cancel,
    Retry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    PasteLast {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    CopyLast {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    Discard {
        session: SessionId,
    },
    /// Opens the quick menu, or closes it.
    Menu,
    Status,
    /// Unlike the others, the connection stays open: a [`Status`] line now and after every change,
    /// with changes within `follow_interval` of each other sent as one line.
    Follow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Response {
        Response { ok: true, message: message.into(), status: None }
    }

    pub fn err(message: impl Into<String>) -> Response {
        Response { ok: false, message: message.into(), status: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: String,
    /// The one new dictations use: client.toml's, or the menu's choice since.
    #[serde(default)]
    pub profile: String,
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<ActiveInfo>,
    /// In delivery order.
    pub finishing: Vec<ActiveInfo>,
    /// Oldest first; includes the dictation in progress (see `in_progress`).
    pub retained: Vec<RetainedInfo>,
    /// A protocol version mismatch, if any, comes first.
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timings: Option<ClientTimings>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveInfo {
    pub session_id: SessionId,
    pub profile: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetainedInfo {
    pub session_id: SessionId,
    pub state: String,
    pub profile: String,
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub gapped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub hint: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_progress: bool,
}

pub fn call(socket: &Path, request: &Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(CLI_TIMEOUT))?;
    stream.set_write_timeout(Some(CLI_TIMEOUT))?;
    let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

impl Status {
    pub fn stopped() -> Status {
        Status {
            state: "stopped".into(),
            profile: String::new(),
            connection: "offline: the daemon isn't running".into(),
            error: None,
            recording: None,
            finishing: Vec::new(),
            retained: Vec::new(),
            warnings: Vec::new(),
            last_timings: None,
        }
    }

    pub fn saved(&self) -> usize {
        self.retained.iter().filter(|r| !r.in_progress).count()
    }

    pub fn render(&self) -> String {
        let mut out = format!("state: {}\nprofile: {}\nserver: {}\n", self.state, self.profile, self.connection);
        if let Some(error) = &self.error {
            out += &format!("error: {error}\n");
        }
        if let Some(rec) = &self.recording {
            out += &format!("recording: {} ({}, {})\n", rec.session_id, rec.profile, rec.phase);
        }
        for item in &self.finishing {
            out += &format!("finishing: {} ({}, {})\n", item.session_id, item.profile, item.phase);
        }
        if self.retained.is_empty() {
            out += "saved recordings: none\n";
        } else {
            out += &format!("saved recordings: {}\n", self.retained.len());
            for rec in &self.retained {
                let gapped = if rec.gapped { ", gapped" } else { "" };
                out += &format!(
                    "  {}  {}  {:.1} s  {}{}\n",
                    rec.session_id,
                    rec.state,
                    rec.duration_ms as f64 / 1000.0,
                    rec.started_at.strftime("%Y-%m-%d %H:%M"),
                    gapped
                );
                if let Some(text) = &rec.text {
                    let preview: String = text.chars().take(60).collect();
                    let more = if text.chars().count() > 60 { "…" } else { "" };
                    out += &format!("    \"{preview}{more}\"\n");
                }
                out += &format!("    run: {}\n", rec.hint);
            }
        }
        for warning in &self.warnings {
            out += &format!("warning: {warning}\n");
        }
        if let Some(t) = &self.last_timings {
            let ms = |v: Option<u32>| v.map_or("-".to_string(), |v| format!("{v} ms"));
            out += &format!(
                "last dictation: stop→final {}, final→paste {}, stop→paste {}\n",
                ms(t.stop_to_final_ms),
                ms(t.final_to_dispatch_ms),
                ms(t.stop_to_dispatch_ms)
            );
        }
        out
    }
}
