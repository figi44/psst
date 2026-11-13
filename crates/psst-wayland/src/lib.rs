//! The daemon's one Wayland thread (plan 5.4, 9.5): the overlay surface, the whole clipboard
//! flow, and the virtual keyboard, on a single connection.
//!
//! The thread owns the clipboard flow: the snapshot, re-snapshotting on external selection
//! changes, the paste, the 5 s restore timer, the generation check, and inheriting a pending
//! restore's snapshot. The daemon only chooses the chord and tracks delivery.
//!
//! This is the contract stub: the messages are final; the thread arrives with the Wayland and
//! overlay work package.

pub use psst_overlay::OverlayState;

/// The paste chord, chosen by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chord {
    CtrlV,
    CtrlShiftV,
}

/// Daemon → Wayland thread.
#[derive(Debug, Clone, PartialEq)]
pub enum WaylandCmd {
    /// Shows or updates the overlay; `None` hides it.
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
    Pasted { generation: u64 },
    Copied { generation: u64 },
    Failed { generation: u64, reason: String },
    Disconnected,
}
