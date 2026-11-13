//! The daemon's side of the Wayland thread (plan 5.4): it sends [`WaylandCmd`]s and receives
//! [`WaylandEvent`]s, and nothing else. The thread itself lives in `psst-wayland`.
//!
//! [`Wayland`] connects lazily and again after `Disconnected`. Tests and the `[insert] record`
//! seam use [`RecordingWayland`], which writes every command to a file instead.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
};

use psst_wayland::{Chord, OverlayState, WaylandCmd, WaylandEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub type WaylandEvents = mpsc::UnboundedSender<WaylandEvent>;

/// Where the daemon sends Wayland commands.
pub trait WaylandSink: Send {
    fn send(&mut self, cmd: WaylandCmd);
}

/// Starts (or restarts) the Wayland thread.
pub trait Wayland: Send {
    /// Connects and returns the command sink; events go to `events`.
    fn connect(&mut self, events: WaylandEvents) -> Result<Box<dyn WaylandSink>, String>;
}

/// The real Wayland thread. Until `psst-wayland` provides it, connecting fails, so every paste
/// reports failure and the text stays available to `psst paste-last`.
pub struct DesktopWayland;

impl Wayland for DesktopWayland {
    fn connect(&mut self, _events: WaylandEvents) -> Result<Box<dyn WaylandSink>, String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("WAYLAND_DISPLAY isn't set".into());
        }
        Err("the Wayland thread (psst-wayland) isn't available in this build".into())
    }
}

/// `[insert] record`: appends each command to a file as one JSON line; pastes and copies succeed
/// at once, and the overlay is "shown" as soon as it's first sent.
pub struct RecordingWayland {
    pub path: PathBuf,
}

impl Wayland for RecordingWayland {
    fn connect(&mut self, events: WaylandEvents) -> Result<Box<dyn WaylandSink>, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(Box::new(RecordingSink { file, events, visible: false }))
    }
}

struct RecordingSink {
    file: File,
    events: WaylandEvents,
    visible: bool,
}

impl WaylandSink for RecordingSink {
    fn send(&mut self, cmd: WaylandCmd) {
        let line = record(&cmd);
        let _ = writeln!(self.file, "{line}");
        let reply = match cmd {
            WaylandCmd::Overlay(Some(_)) if !self.visible => {
                self.visible = true;
                Some(WaylandEvent::OverlayShown)
            }
            WaylandCmd::Overlay(state) => {
                self.visible = state.is_some();
                None
            }
            WaylandCmd::SnapshotClipboard { .. } => None,
            WaylandCmd::Paste { generation, .. } => Some(WaylandEvent::Pasted { generation }),
            WaylandCmd::Copy { generation, .. } => Some(WaylandEvent::Copied { generation }),
        };
        if let Some(event) = reply {
            let _ = self.events.send(event);
        }
    }
}

/// One command as the `[insert] record` file writes it.
pub fn record(cmd: &WaylandCmd) -> Value {
    match cmd {
        WaylandCmd::Overlay(state) => json!({"cmd": "overlay", "state": state.as_ref().map(overlay_json)}),
        WaylandCmd::SnapshotClipboard { generation } => json!({"cmd": "snapshot", "generation": generation}),
        WaylandCmd::Paste { generation, text, chord, delay_ms } => json!({
            "cmd": "paste", "generation": generation, "text": text,
            "chord": match chord { Chord::CtrlV => "ctrl+v", Chord::CtrlShiftV => "ctrl+shift+v" },
            "delay_ms": delay_ms,
        }),
        WaylandCmd::Copy { generation, text } => json!({"cmd": "copy", "generation": generation, "text": text}),
    }
}

fn overlay_json(state: &OverlayState) -> Value {
    match state {
        OverlayState::Starting => json!({"kind": "starting"}),
        OverlayState::Recording { level } => json!({"kind": "recording", "level": level}),
        OverlayState::Offline { level, reason } => json!({"kind": "offline", "level": level, "reason": reason}),
        OverlayState::LoadingModel => json!({"kind": "loading_model"}),
        OverlayState::Processing => json!({"kind": "processing"}),
        OverlayState::Sending => json!({"kind": "sending"}),
        OverlayState::Retrying => json!({"kind": "retrying"}),
        OverlayState::WaitingForServer => json!({"kind": "waiting_for_server"}),
        OverlayState::NothingHeard => json!({"kind": "nothing_heard"}),
        OverlayState::Done => json!({"kind": "done"}),
        OverlayState::Error { message } => json!({"kind": "error", "message": message}),
        OverlayState::Flash { message } => json!({"kind": "flash", "message": message}),
        OverlayState::LimitWarning { seconds_left } => json!({"kind": "limit_warning", "seconds_left": seconds_left}),
    }
}
