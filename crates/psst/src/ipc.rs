//! The daemon's command socket, `$XDG_RUNTIME_DIR/psst/daemon.sock` (plan 9.1): one JSON
//! request line, one JSON response line, then the connection closes. The CLI side is blocking
//! std I/O, so a command costs a connect and a round trip.

use std::{
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use jiff::Timestamp;
use psst_proto::{ClientTimings, SessionId};
use serde::{Deserialize, Serialize};

/// How long the CLI waits for the daemon.
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
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// For people: what happened, or why not.
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

/// `psst status --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// `idle`, `starting`, `recording`, `finishing` or `inserting`.
    pub state: String,
    /// `connected`, `connecting`, or `offline: <reason>`.
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<ActiveInfo>,
    /// Dictations after `stop`, in delivery order.
    pub finishing: Vec<ActiveInfo>,
    /// Recordings kept on disk, oldest first.
    pub retained: Vec<RetainedInfo>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timings: Option<ClientTimings>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveInfo {
    pub session_id: SessionId,
    pub profile: String,
    /// What it's doing, for example `recording (offline: …)` or `uploading`.
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetainedInfo {
    pub session_id: SessionId,
    /// `recording`, `complete`, `incomplete` or `transcribed`.
    pub state: String,
    pub profile: String,
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub gapped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The command that delivers or retries it.
    pub hint: String,
}

/// Sends one request to the daemon and waits for its response.
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
    /// The human-readable form of `psst status`.
    pub fn render(&self) -> String {
        let mut out = format!("state: {}\nserver: {}\n", self.state, self.connection);
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
