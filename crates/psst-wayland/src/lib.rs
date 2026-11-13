//! Paste order, on the one connection: data-control `set_selection` → `wl_display.sync` →
//! `delay_ms` → the chord through the virtual keyboard → `wl_display.sync` →
//! [`WaylandEvent::Pasted`].
//!
//! Idle, the thread sleeps in `epoll` with no timers; only the waveform wakes it on its own.

use std::{io, path::PathBuf, thread::JoinHandle, time::Duration};

pub mod clipboard;
mod dc;
mod keymap;
mod overlay;
mod thread;

pub use psst_overlay::{OverlayState, Rgba, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chord {
    CtrlV,
    CtrlShiftV,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaylandCmd {
    /// A state with a level shows the waveform, which eases towards each new level and keeps moving
    /// between updates.
    Overlay(Option<OverlayState>),
    /// Sent at `stop`.
    SnapshotClipboard {
        generation: u64,
    },
    Paste {
        generation: u64,
        text: String,
        chord: Chord,
        delay_ms: u32,
    },
    Copy {
        generation: u64,
        text: String,
    },
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    Top,
    #[default]
    Bottom,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayConfig {
    pub anchor: Anchor,
    /// Logical pixels, as are `width` and `height`.
    pub margin: u32,
    pub width: u32,
    pub height: u32,
    pub style: Style,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self { anchor: Anchor::Bottom, margin: 130, width: 240, height: 44, style: Style::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataControl {
    #[default]
    Auto,
    Ext,
    Wlr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub overlay: OverlayConfig,
    pub display: Option<PathBuf>,
    pub data_control: DataControl,
    /// After `Pasted`; tests shorten it.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Wayland thread has stopped")
    }
}

impl std::error::Error for Stopped {}

/// Dropping it abandons a pending clipboard restore.
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
    /// `on_event` runs on the Wayland thread, so it should only hand the event on.
    pub fn spawn(options: Options, on_event: impl FnMut(WaylandEvent) + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = calloop::channel::channel();
        let join = std::thread::Builder::new()
            .name("psst-wayland".into())
            .spawn(move || thread::run(options, rx, Box::new(on_event)))?;
        Ok(Self { tx, join: Some(join) })
    }

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
