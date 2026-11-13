//! Rendering is pure: the same arguments always give the same pixels. The waveform moves only as
//! the caller advances a [`Meter`] and passes on its [`Wave`].

use std::{f32::consts::TAU, time::Duration};

mod menu;
mod render;

pub use menu::{Key, Listing, MENU_WIDTH, Menu, MenuAction, MenuOutcome, MenuSpec, ModelChoice};
pub use render::Renderer;
pub use tiny_skia::Pixmap;

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayState {
    /// Until the first audio frame, so the overlay never claims to record early.
    Starting,
    Recording {
        level: f32,
    },
    Offline {
        level: f32,
        reason: String,
    },
    LoadingModel,
    Processing,
    Sending,
    Retrying,
    WaitingForServer,
    NothingHeard,
    Done,
    Error {
        message: String,
    },
    Flash {
        message: String,
    },
    LimitWarning {
        level: f32,
        seconds_left: u32,
    },
}

impl OverlayState {
    /// `Some` for the states that show the waveform.
    pub fn level(&self) -> Option<f32> {
        match self {
            OverlayState::Recording { level }
            | OverlayState::Offline { level, .. }
            | OverlayState::LimitWarning { level, .. } => Some(*level),
            _ => None,
        }
    }
}

/// Straight (not premultiplied) alpha.
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

    pub fn faded(self, factor: f32) -> Self {
        Self { a: (self.a as f32 * factor.clamp(0.0, 1.0)).round() as u8, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Style {
    pub background: Rgba,
    pub foreground: Rgba,
    pub accent: Rgba,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            background: Rgba::new(0x00, 0x00, 0x00, 0xff),
            foreground: Rgba::new(0xf5, 0xf5, 0xf7, 0xff),
            accent: Rgba::new(0xff, 0x45, 0x3a, 0xff),
        }
    }
}

/// One moment of the waveform.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Wave {
    /// The microphone level, eased.
    pub level: f32,
    /// How far the ripple has travelled, in radians.
    pub phase: f32,
}

/// Seconds to close most of the gap to a louder level: syllables show at once.
const ATTACK: f32 = 0.04;
/// Seconds to fall back: the wave settles instead of flickering between words.
const RELEASE: f32 = 0.2;
/// Radians a second: a slow sway in silence, quicker while speaking.
const QUIET_SPEED: f32 = 2.4;
const LOUD_SPEED: f32 = 5.0;
/// The renderer's ripples turn at 1 and 3/5 of the phase, so both are whole turns here.
const PHASE_PERIOD: f32 = 5.0 * TAU;

/// Turns the level updates the daemon sends every few frames into a wave that moves smoothly
/// between them and keeps moving through silence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meter {
    target: f32,
    wave: Wave,
}

impl Meter {
    pub fn set(&mut self, level: f32) {
        self.target = if level.is_nan() { 0.0 } else { level.clamp(0.0, 1.0) };
    }

    pub fn advance(&mut self, elapsed: Duration) -> Wave {
        // A stalled frame resumes where it was instead of jumping.
        let dt = elapsed.as_secs_f32().min(0.1);
        let wave = &mut self.wave;
        let tau = if self.target > wave.level { ATTACK } else { RELEASE };
        wave.level += (self.target - wave.level) * (1.0 - (-dt / tau).exp());
        wave.phase = (wave.phase + dt * (QUIET_SPEED + LOUD_SPEED * wave.level)) % PHASE_PERIOD;
        *wave
    }

    pub fn wave(&self) -> Wave {
        self.wave
    }
}

pub fn physical_size(width: u32, height: u32, scale: f64) -> (u32, u32) {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let px = |v: u32| ((v.max(1) as f64 * scale).round() as u32).max(1);
    (px(width), px(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(meter: &mut Meter, ms: u64) -> Wave {
        for _ in 0..ms / 16 {
            meter.advance(Duration::from_millis(16));
        }
        meter.wave()
    }

    #[test]
    fn the_wave_rises_fast_falls_slowly_and_keeps_moving_in_silence() {
        let mut meter = Meter::default();
        meter.set(0.8);
        assert!(run(&mut meter, 96).level > 0.7, "a syllable shows within a few frames");
        meter.set(0.0);
        let after_word = run(&mut meter, 96).level;
        assert!((0.2..0.6).contains(&after_word), "the wave settles between words: {after_word}");
        let still = run(&mut meter, 2000);
        assert!(still.level < 0.01);
        assert_ne!(run(&mut meter, 160).phase, still.phase, "silence still ripples");
    }

    #[test]
    fn the_meter_ignores_bad_levels_and_stalls() {
        let mut meter = Meter::default();
        meter.set(f32::NAN);
        assert_eq!(meter.advance(Duration::from_millis(16)).level, 0.0);
        meter.set(7.0);
        let wave = meter.advance(Duration::from_secs(3600));
        assert!(wave.level <= 1.0 && wave.phase < PHASE_PERIOD, "{wave:?}");
        assert!(wave.phase <= 0.1 * (QUIET_SPEED + LOUD_SPEED), "a stall moves it one short step: {wave:?}");
    }

    #[test]
    fn physical_size_rounds_fractional_scales() {
        assert_eq!(physical_size(240, 44, 1.0), (240, 44));
        assert_eq!(physical_size(240, 44, 1.25), (300, 55));
        assert_eq!(physical_size(240, 44, 1.5), (360, 66));
        assert_eq!(physical_size(0, 44, f64::NAN), (1, 44));
    }
}
