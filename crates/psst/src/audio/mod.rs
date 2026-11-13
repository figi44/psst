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
pub mod gaps;
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
    /// Silence owed for samples that didn't fit in the ring (at the source's rate). The capture
    /// side pushes it, in capture order, before any newer samples; the recorder appends what's
    /// still owed once capture has stopped.
    owed_silence: AtomicU64,
    /// A glitch whose length is unknown (an xrun).
    glitched: AtomicBool,
    /// Capture callbacks so far.
    callbacks: AtomicU64,
    /// Why capture failed for good (a lost device).
    failed: Mutex<Option<String>>,
}

impl FeedShared {
    /// Silence still owed; call only once capture has stopped.
    pub fn take_owed_silence(&self) -> u64 {
        self.owed_silence.swap(0, Ordering::AcqRel)
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

    /// A glitch whose length is unknown.
    pub fn glitch(&self) {
        self.glitched.store(true, Ordering::Release);
    }

    /// Capture can't continue (for example the device is gone). The first reason is kept.
    pub fn fail(&self, reason: impl FnOnce() -> String) {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.get_or_insert_with(reason);
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

    /// Pushes captured samples. What doesn't fit is replaced by silence in its place: the
    /// recording keeps its length and order.
    pub fn push(&mut self, samples: &[f32]) {
        if !self.pay_silence() {
            self.owe(samples.len());
            return;
        }
        let (_, rest) = self.producer.push_partial_slice(samples);
        if !rest.is_empty() {
            self.owe(rest.len());
        }
    }

    /// Pushes `n` samples of silence for a discontinuity of known length.
    pub fn push_silence(&mut self, n: usize) {
        self.owe(n);
        self.pay_silence();
    }

    fn owe(&self, n: usize) {
        self.shared.owed_silence.fetch_add(n as u64, Ordering::AcqRel);
        self.shared.glitched.store(true, Ordering::Release);
    }

    /// Pushes the silence owed so far, as far as it fits; `true` once nothing is owed.
    fn pay_silence(&mut self) -> bool {
        const ZEROS: [f32; 256] = [0.0; 256];
        let mut owed = self.shared.owed_silence.load(Ordering::Acquire);
        while owed > 0 {
            let n = (owed as usize).min(ZEROS.len()).min(self.producer.slots());
            if n == 0 {
                return false;
            }
            let _ = self.producer.push_partial_slice(&ZEROS[..n]);
            self.shared.owed_silence.fetch_sub(n as u64, Ordering::AcqRel);
            owed -= n as u64;
        }
        true
    }

    /// A glitch of unknown length.
    pub fn glitch(&self) {
        self.shared.glitch();
    }

    /// Counts a capture callback, for draining at stop.
    pub fn tick(&self) {
        self.shared.callbacks.fetch_add(1, Ordering::AcqRel);
    }

    /// Capture can't continue (for example the device is gone).
    pub fn fail(&self, reason: String) {
        self.shared.fail(|| reason);
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
    fn samples_that_overflow_become_silence_in_their_place() {
        let (producer, mut ring) = rtrb::RingBuffer::new(4);
        let shared = Arc::new(FeedShared::default());
        let mut feed = Feed::new(producer, shared.clone());
        feed.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]); // 5 and 6 don't fit
        let mut out = vec![0.0; 4];
        let _ = ring.pop_partial_slice(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        feed.push(&[7.0, 8.0, 9.0]); // the owed silence goes first; 9 doesn't fit
        let _ = ring.pop_partial_slice(&mut out);
        assert_eq!(out, [0.0, 0.0, 7.0, 8.0]);
        assert!(shared.take_glitched());
        assert_eq!(shared.take_owed_silence(), 1, "still owed for 9, appended at the end");
    }

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
