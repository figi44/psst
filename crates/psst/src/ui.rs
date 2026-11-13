//! The daemon's side of the Wayland thread (plan 5.4): it sends [`WaylandCmd`]s and receives
//! [`WaylandEvent`]s, and nothing else. The thread itself lives in `psst-wayland`.
//!
//! The daemon asks [`Wayland`] for a sink when it has none: at the first command, and again
//! after `Disconnected`. [`DesktopWayland`] is the real thread; tests and the `[insert] record`
//! seam use [`RecordingWayland`], which writes every command to a file instead.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use psst_config::{Anchor as ConfigAnchor, Color};
use psst_wayland::{Anchor, Chord, OverlayConfig, OverlayState, Rgba, Style, WaylandCmd, WaylandEvent, WaylandThread};
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

/// `client.toml`'s `[overlay]` as the Wayland thread takes it.
pub fn overlay_config(cfg: &psst_config::OverlayConfig) -> OverlayConfig {
    let rgba = |c: Color| Rgba::new(c.r, c.g, c.b, c.a);
    OverlayConfig {
        anchor: match cfg.anchor {
            ConfigAnchor::Top => Anchor::Top,
            ConfigAnchor::Bottom => Anchor::Bottom,
        },
        margin: cfg.margin,
        width: cfg.width,
        height: cfg.height,
        style: Style { background: rgba(cfg.background), foreground: rgba(cfg.foreground), accent: rgba(cfg.accent) },
    }
}

/// The real Wayland thread (`psst-wayland`).
///
/// [`DesktopWayland::new`] starts it with the daemon, so the overlay's font system is built once,
/// before the first `toggle`. The thread connects to the compositor on its first command, and
/// again on the next command after it lost the compositor, so the daemon dropping its sink on
/// `Disconnected` keeps the thread. A thread that has stopped (it panicked) is started again by
/// the next [`Wayland::connect`].
pub struct DesktopWayland {
    options: psst_wayland::Options,
    running: Option<Arc<Running>>,
}

struct Running {
    thread: WaylandThread,
    /// Where the thread's events go: the sender from the latest `connect`.
    events: Arc<Mutex<Option<WaylandEvents>>>,
    stopped: AtomicBool,
}

impl DesktopWayland {
    /// Starts the thread with `client.toml`'s `[overlay]`. It connects to `WAYLAND_DISPLAY`
    /// (relative to `XDG_RUNTIME_DIR`) on its first command.
    pub fn new(overlay: &psst_config::OverlayConfig) -> Self {
        let options = psst_wayland::Options { overlay: overlay_config(overlay), ..psst_wayland::Options::default() };
        let mut wayland = DesktopWayland { options, running: None };
        if let Err(err) = wayland.start() {
            tracing::warn!("{err}");
        }
        wayland
    }

    fn start(&mut self) -> Result<Arc<Running>, String> {
        let events: Arc<Mutex<Option<WaylandEvents>>> = Arc::default();
        let forward = events.clone();
        let thread = WaylandThread::spawn(self.options.clone(), move |event| {
            if let Some(tx) = forward.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let _ = tx.send(event);
            }
        })
        .map_err(|err| format!("cannot start the Wayland thread: {err}"))?;
        let running = Arc::new(Running { thread, events, stopped: AtomicBool::new(false) });
        self.running = Some(running.clone());
        Ok(running)
    }
}

impl Wayland for DesktopWayland {
    fn connect(&mut self, events: WaylandEvents) -> Result<Box<dyn WaylandSink>, String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("WAYLAND_SOCKET").is_none() {
            return Err("WAYLAND_DISPLAY isn't set".into());
        }
        let running = match &self.running {
            Some(running) if !running.stopped.load(Ordering::Acquire) => running.clone(),
            _ => self.start()?,
        };
        *running.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(events.clone());
        Ok(Box::new(DesktopSink { running, events, dead: false }))
    }
}

struct DesktopSink {
    running: Arc<Running>,
    events: WaylandEvents,
    /// The thread had stopped when a command was sent; `Disconnected` went out once.
    dead: bool,
}

impl WaylandSink for DesktopSink {
    fn send(&mut self, cmd: WaylandCmd) {
        let generation = match &cmd {
            WaylandCmd::Paste { generation, .. } | WaylandCmd::Copy { generation, .. } => Some(*generation),
            WaylandCmd::Overlay(_) | WaylandCmd::SnapshotClipboard { .. } => None,
        };
        if let Err(stopped) = self.running.thread.send(cmd) {
            self.running.stopped.store(true, Ordering::Release);
            if let Some(generation) = generation {
                let _ = self.events.send(WaylandEvent::Failed { generation, reason: stopped.to_string() });
            }
            if !std::mem::replace(&mut self.dead, true) {
                tracing::warn!("{stopped}; starting it again");
                let _ = self.events.send(WaylandEvent::Disconnected);
            }
        }
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

#[cfg(test)]
mod tests {
    use psst_config::{Anchor as ConfigAnchor, Color};
    use psst_wayland::{Anchor, OverlayConfig, Rgba, Style};

    use super::overlay_config;

    #[test]
    fn the_default_overlay_is_the_wayland_threads_default() {
        assert_eq!(overlay_config(&psst_config::OverlayConfig::default()), OverlayConfig::default());
    }

    #[test]
    fn overlay_settings_reach_the_wayland_thread() {
        let cfg = psst_config::OverlayConfig {
            anchor: ConfigAnchor::Top,
            margin: 12,
            width: 300,
            height: 50,
            background: Color { r: 1, g: 2, b: 3, a: 4 },
            foreground: Color { r: 5, g: 6, b: 7, a: 255 },
            accent: Color { r: 8, g: 9, b: 10, a: 128 },
        };
        assert_eq!(
            overlay_config(&cfg),
            OverlayConfig {
                anchor: Anchor::Top,
                margin: 12,
                width: 300,
                height: 50,
                style: Style {
                    background: Rgba::new(1, 2, 3, 4),
                    foreground: Rgba::new(5, 6, 7, 255),
                    accent: Rgba::new(8, 9, 10, 128),
                },
            }
        );
    }
}
