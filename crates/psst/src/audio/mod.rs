//! Audio capture (plan 9.2, 9.3).
//!
//! An [`AudioSource`] captures mono `f32` at its own rate into a lock-free ring buffer (the
//! [`Feed`]). The recorder thread ([`recorder::start`]) drains it, resamples to 16 kHz with
//! rubato, cuts 20 ms frames, writes them to the recording's WAV from the very first frame, and
//! hands each one to the daemon with its level.
//!
//! - **Glitches don't stop a recording.** A ring-buffer overflow or a capture discontinuity is
//!   filled with silence and the recording is flagged `gapped`.
//! - **Only a failed write or a lost device stops it.**
//! - **At stop** the recorder waits for one more capture callback, drains the ring and flushes
//!   the resampler, so the last syllable isn't cut off.

mod cpal_source;
mod file_source;
pub mod recorder;

use std::{
    any::Any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub use cpal_source::CpalSource;
pub use file_source::FileSource;
use psst_config::AudioInput;

/// Ring-buffer capacity in samples: about 2.7 s at 48 kHz.
pub const RING_SAMPLES: usize = 1 << 17;

/// Captures audio for one recording.
pub trait AudioSource: Send + Sync + 'static {
    /// Starts capturing into `feed`. Called on the recorder thread; capture stops when the
    /// returned [`OpenCapture`] is dropped.
    fn open(&self, feed: Feed) -> Result<OpenCapture, String>;
}

/// A running capture.
pub struct OpenCapture {
    /// Rate of the mono samples pushed into the feed.
    pub sample_rate: u32,
    /// Whether the recorder should wait for one more callback at stop (real devices) or not
    /// (the file source, which has nothing buffered).
    pub drain_on_stop: bool,
    /// Keeps the capture alive.
    pub guard: Box<dyn Any>,
}

/// The source for `audio.input`.
pub fn source_for(input: &AudioInput) -> Arc<dyn AudioSource> {
    match input {
        AudioInput::Default => Arc::new(CpalSource { device: None }),
        AudioInput::Device(name) => Arc::new(CpalSource { device: Some(name.clone()) }),
        AudioInput::File(path) => Arc::new(FileSource { path: path.clone() }),
    }
}

/// What the capture callback and the recorder share besides the ring buffer.
#[derive(Debug, Default)]
pub struct FeedShared {
    /// Samples that didn't fit in the ring (at the source's rate); filled with silence.
    dropped: AtomicU64,
    /// A glitch whose length is unknown (an xrun).
    glitched: AtomicBool,
    /// Capture callbacks so far.
    callbacks: AtomicU64,
    /// Why capture failed for good (a lost device).
    failed: Mutex<Option<String>>,
}

impl FeedShared {
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::AcqRel)
    }

    pub fn take_glitched(&self) -> bool {
        self.glitched.swap(false, Ordering::AcqRel)
    }

    pub fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Acquire)
    }

    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The capture side of the ring buffer. Never blocks.
pub struct Feed {
    producer: rtrb::Producer<f32>,
    shared: Arc<FeedShared>,
}

impl Feed {
    pub fn new(producer: rtrb::Producer<f32>, shared: Arc<FeedShared>) -> Feed {
        Feed { producer, shared }
    }

    /// Pushes captured samples; what doesn't fit is counted and later filled with silence.
    pub fn push(&mut self, samples: &[f32]) {
        let (_, rest) = self.producer.push_partial_slice(samples);
        if !rest.is_empty() {
            self.shared.dropped.fetch_add(rest.len() as u64, Ordering::AcqRel);
        }
    }

    /// Pushes `n` samples of silence for a discontinuity of known length.
    pub fn push_silence(&mut self, n: usize) {
        const ZEROS: [f32; 256] = [0.0; 256];
        let mut left = n;
        while left > 0 {
            let take = left.min(ZEROS.len());
            self.push(&ZEROS[..take]);
            left -= take;
        }
        self.shared.glitched.store(true, Ordering::Release);
    }

    /// A glitch of unknown length.
    pub fn glitch(&self) {
        self.shared.glitched.store(true, Ordering::Release);
    }

    /// Counts a capture callback, for draining at stop.
    pub fn tick(&self) {
        self.shared.callbacks.fetch_add(1, Ordering::AcqRel);
    }

    /// Capture can't continue (for example the device is gone).
    pub fn fail(&self, reason: String) {
        let mut failed = self.shared.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.get_or_insert(reason);
    }

    pub fn shared(&self) -> &Arc<FeedShared> {
        &self.shared
    }
}

/// The level of a frame for the meter: RMS in dBFS mapped from −60…0 dB to 0…1.
pub fn level(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    let rms = (sum / frame.len() as f64).sqrt();
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_meter_spans_silence_to_full_scale() {
        assert_eq!(level(&[0; 320]), 0.0);
        assert!(level(&[i16::MAX; 320]) > 0.99);
        let quiet = level(&[33; 320]); // about −60 dBFS
        assert!(quiet < 0.05, "{quiet}");
        let speech = level(&[3_000; 320]); // about −21 dBFS
        assert!((0.5..0.8).contains(&speech), "{speech}");
    }
}
