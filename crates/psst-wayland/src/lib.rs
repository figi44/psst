//! The daemon's one Wayland thread (plan 5.4, 9.2, 9.5, 9.6): the overlay surface, the whole
//! clipboard flow, and the virtual keyboard, on a single connection and one calloop loop.
//!
//! The thread owns the clipboard flow: the snapshot, re-snapshotting on external selection
//! changes, the paste, the 5 s restore timer, the generation check, and inheriting a pending
//! restore's snapshot ([`clipboard`] has the rules). The daemon only chooses the chord and
//! tracks delivery.
//!
//! ```no_run
//! use psst_wayland::{Chord, Options, WaylandCmd, WaylandThread};
//!
//! let (tx, rx) = std::sync::mpsc::channel();
//! let wayland = WaylandThread::spawn(Options::default(), move |event| {
//!     let _ = tx.send(event);
//! })?;
//! wayland.send(WaylandCmd::SnapshotClipboard { generation: 1 })?;
//! wayland.send(WaylandCmd::Paste { generation: 1, text: "Hello".into(), chord: Chord::CtrlV, delay_ms: 0 })?;
//! let pasted = rx.recv()?; // WaylandEvent::Pasted { generation: 1 }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! **Connection.** The thread connects lazily, on the first command, and keeps the connection.
//! If the compositor goes away it reports [`WaylandEvent::Disconnected`] (failing any paste or
//! copy in progress), and connects again on the next command. A command that can't connect fails
//! its paste or copy with [`WaylandEvent::Failed`]; overlay updates are dropped.
//!
//! **Paste order**, on the one connection: data-control `set_selection` (the text under all five
//! plain-text types) → `wl_display.sync` → `delay_ms` → the chord through a virtual keyboard
//! with psst's own keymap → `wl_display.sync` → [`WaylandEvent::Pasted`]. Data control prefers
//! `ext_data_control_manager_v1` and falls back to `zwlr_data_control_manager_v1`.
//!
//! **Overlay.** A layer-shell surface on the overlay layer, with no keyboard focus and an empty
//! input region, scaled with `wp_fractional_scale_v1` and `wp_viewporter`. It draws only when a
//! new state arrives (throttled by frame callbacks) and is destroyed when hidden.
//! [`WaylandEvent::OverlayShown`] follows the first presented frame (`wp_presentation`) after
//! it appears.
//!
//! **Idle** the thread sleeps in `epoll` with no timers. A snapshot nobody pastes is freed after
//! two minutes.

use std::{io, path::PathBuf, thread::JoinHandle, time::Duration};

pub mod clipboard;
mod dc;
mod keymap;
mod overlay;
mod thread;

pub use psst_overlay::{OverlayState, Rgba, Style};

/// The paste chord, chosen by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chord {
    CtrlV,
    CtrlShiftV,
}

/// Daemon → Wayland thread.
#[derive(Debug, Clone, PartialEq)]
pub enum WaylandCmd {
    /// Shows or updates the overlay; `None` hides it. Every `Recording` or `Offline` update adds
    /// its level to the waveform as one bar, so send them at a steady pace (every 40–60 ms).
    Overlay(Option<OverlayState>),
    /// Takes a clipboard snapshot for the dictation with this generation (sent at `stop`).
    SnapshotClipboard { generation: u64 },
    /// Sets the selection to `text`, waits `delay_ms`, presses `chord`, and restores the
    /// snapshot 5 s later.
    Paste { generation: u64, text: String, chord: Chord, delay_ms: u32 },
    /// Copy only: no paste, no restore.
    Copy { generation: u64, text: String },
}

/// Wayland thread → daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaylandEvent {
    /// The overlay's first commit was presented.
    OverlayShown,
    Pasted {
        generation: u64,
    },
    Copied {
        generation: u64,
    },
    Failed {
        generation: u64,
        reason: String,
    },
    Disconnected,
}

/// Screen edge the overlay sits on, centred horizontally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    Top,
    #[default]
    Bottom,
}

/// Where the overlay sits and how it looks: `[overlay]` in `client.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayConfig {
    pub anchor: Anchor,
    /// Distance from the anchored edge, in logical pixels.
    pub margin: u32,
    /// Size in logical pixels.
    pub width: u32,
    pub height: u32,
    pub style: Style,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self { anchor: Anchor::Bottom, margin: 130, width: 240, height: 44, style: Style::default() }
    }
}

/// Which data-control protocol to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataControl {
    /// `ext_data_control_manager_v1` if offered, else `zwlr_data_control_manager_v1`.
    #[default]
    Auto,
    Ext,
    Wlr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub overlay: OverlayConfig,
    /// The compositor's socket. `None` connects like any client: `WAYLAND_SOCKET`, or
    /// `WAYLAND_DISPLAY` (relative to `XDG_RUNTIME_DIR`).
    pub display: Option<PathBuf>,
    pub data_control: DataControl,
    /// How long after `Pasted` the clipboard is restored. 5 s; tests shorten it.
    pub restore_after: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            overlay: OverlayConfig::default(),
            display: None,
            data_control: DataControl::Auto,
            restore_after: clipboard::Config::default().restore_after,
        }
    }
}

/// The thread has stopped (it panicked), so it can't take commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Wayland thread has stopped")
    }
}

impl std::error::Error for Stopped {}

/// The running Wayland thread. Dropping it stops the thread: the overlay disappears and a pending
/// clipboard restore is abandoned.
pub struct WaylandThread {
    tx: calloop::channel::Sender<thread::Msg>,
    join: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WaylandThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandThread").finish_non_exhaustive()
    }
}

impl WaylandThread {
    /// Starts the thread. It builds the overlay's font system now and connects to the
    /// compositor on the first command. `on_event` runs on the Wayland thread, so it should only
    /// hand the event on (for example into a channel).
    pub fn spawn(options: Options, on_event: impl FnMut(WaylandEvent) + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = calloop::channel::channel();
        let join = std::thread::Builder::new()
            .name("psst-wayland".into())
            .spawn(move || thread::run(options, rx, Box::new(on_event)))?;
        Ok(Self { tx, join: Some(join) })
    }

    /// Queues a command. Commands run in order.
    pub fn send(&self, cmd: WaylandCmd) -> Result<(), Stopped> {
        self.tx.send(thread::Msg::Cmd(cmd)).map_err(|_| Stopped)
    }
}

impl Drop for WaylandThread {
    fn drop(&mut self) {
        let _ = self.tx.send(thread::Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
