//! The overlay's pure renderer: (`OverlayState`, recent levels, style, size, scale) → RGBA pixels.
//!
//! The overlay is a pill, centred at the bottom of the screen by default. A round "well" on its
//! left holds the status icon; the rest shows a live level waveform while recording, or a short
//! label. [`Renderer::render`] draws one frame; the Wayland thread (`psst-wayland`) puts it on a
//! layer-shell surface.
//!
//! - The font system holds one embedded font, Inter 4.1 Medium (SIL Open Font License 1.1, see
//!   `fonts/Inter-LICENSE.txt`). It's built once, in [`Renderer::new`].
//! - The output depends only on the arguments: the same state, levels, style, size and scale
//!   always give the same pixels. Snapshot tests in `tests/snapshots.rs` hold every state.
//! - Nothing animates on its own. The waveform moves only as new levels arrive.

use std::collections::VecDeque;

mod render;

pub use render::Renderer;
pub use tiny_skia::Pixmap;

/// What the overlay shows. `None` in `WaylandCmd::Overlay` hides it.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayState {
    /// From `toggle` until the first audio frame, so the overlay never claims to record early.
    Starting,
    /// `level` is the input level, 0.0–1.0.
    Recording {
        level: f32,
    },
    /// Recording to the file while the server is unreachable or rejected the session.
    Offline {
        level: f32,
        reason: String,
    },
    LoadingModel,
    Processing,
    /// Uploading a recording.
    Sending,
    Retrying,
    WaitingForServer,
    NothingHeard,
    Done,
    Error {
        message: String,
    },
    /// A short notice, for example an ignored toggle or a gapped recording.
    Flash {
        message: String,
    },
    /// Shown from 30 s before `max_recording_s`.
    LimitWarning {
        seconds_left: u32,
    },
}

impl OverlayState {
    /// The input level this state carries, if it's a recording state with one.
    pub fn level(&self) -> Option<f32> {
        match self {
            OverlayState::Recording { level } | OverlayState::Offline { level, .. } => Some(*level),
            _ => None,
        }
    }
}

/// An sRGB colour with straight (not premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// The same colour with its alpha multiplied by `factor` (0.0–1.0).
    pub fn faded(self, factor: f32) -> Self {
        Self { a: (self.a as f32 * factor.clamp(0.0, 1.0)).round() as u8, ..self }
    }
}

/// The overlay's colours: `[overlay]` in `client.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Style {
    pub background: Rgba,
    /// Text and icons.
    pub foreground: Rgba,
    /// The recording indicator and level meter.
    pub accent: Rgba,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            background: Rgba::new(0x1c, 0x1c, 0x1e, 0xe6),
            foreground: Rgba::new(0xf5, 0xf5, 0xf7, 0xff),
            accent: Rgba::new(0xff, 0x45, 0x3a, 0xff),
        }
    }
}

/// How many levels the waveform keeps.
pub const LEVEL_HISTORY: usize = 64;

/// Recent input levels for the waveform, oldest first. The Wayland thread pushes one for every
/// `Recording` or `Offline` update and clears them when the overlay is hidden.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Levels {
    values: VecDeque<f32>,
}

impl Levels {
    pub fn new() -> Self {
        Self { values: VecDeque::with_capacity(LEVEL_HISTORY) }
    }

    /// Adds the newest level, clamped to 0.0–1.0 (NaN counts as silence), dropping the oldest
    /// beyond [`LEVEL_HISTORY`].
    pub fn push(&mut self, level: f32) {
        if self.values.len() == LEVEL_HISTORY {
            self.values.pop_front();
        }
        self.values.push_back(if level.is_nan() { 0.0 } else { level.clamp(0.0, 1.0) });
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = f32> + ExactSizeIterator + '_ {
        self.values.iter().copied()
    }
}

impl FromIterator<f32> for Levels {
    fn from_iter<I: IntoIterator<Item = f32>>(iter: I) -> Self {
        let mut levels = Levels::new();
        for level in iter {
            levels.push(level);
        }
        levels
    }
}

/// The buffer size, in physical pixels, for a surface of `width` × `height` logical pixels at
/// `scale` (fractional scales included). Never zero.
pub fn physical_size(width: u32, height: u32, scale: f64) -> (u32, u32) {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let px = |v: u32| ((v.max(1) as f64 * scale).round() as u32).max(1);
    (px(width), px(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_keep_the_newest_and_clamp() {
        let mut levels = Levels::new();
        for i in 0..LEVEL_HISTORY + 3 {
            levels.push(i as f32 / 10.0);
        }
        assert_eq!(levels.len(), LEVEL_HISTORY);
        assert_eq!(levels.iter().next(), Some(0.3));
        assert_eq!(levels.iter().last(), Some(1.0));
        levels.push(f32::NAN);
        levels.push(-2.0);
        assert_eq!(levels.iter().rev().take(2).collect::<Vec<_>>(), [0.0, 0.0]);
    }

    #[test]
    fn physical_size_rounds_fractional_scales() {
        assert_eq!(physical_size(240, 44, 1.0), (240, 44));
        assert_eq!(physical_size(240, 44, 1.25), (300, 55));
        assert_eq!(physical_size(240, 44, 1.5), (360, 66));
        assert_eq!(physical_size(0, 44, f64::NAN), (1, 44));
    }
}
