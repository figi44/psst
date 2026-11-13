//! The overlay's pure renderer: (`OverlayState`, size, scale) → RGBA pixels (plan 9.6).
//!
//! This is the contract stub: [`OverlayState`] is final; the renderer arrives with the Wayland
//! and overlay work package.

/// What the overlay shows. `None` in `WaylandCmd::Overlay` hides it.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayState {
    /// From `toggle` until the first audio frame, so the overlay never claims to record early.
    Starting,
    /// `level` is the input level, 0.0–1.0.
    Recording { level: f32 },
    /// Recording to the file while the server is unreachable or rejected the session.
    Offline { level: f32, reason: String },
    LoadingModel,
    Processing,
    /// Uploading a recording.
    Sending,
    Retrying,
    WaitingForServer,
    NothingHeard,
    Done,
    Error { message: String },
    /// A short notice, for example an ignored toggle or a gapped recording.
    Flash { message: String },
    /// Shown from 30 s before `max_recording_s`.
    LimitWarning { seconds_left: u32 },
}
