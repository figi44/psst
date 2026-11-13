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

/// About 2.7 s at 48 kHz.
pub const RING_SAMPLES: usize = 1 << 17;

pub trait AudioSource: Send + Sync + 'static {
    /// Called on the recorder thread; capture stops when the [`OpenCapture`] is dropped.
    fn open(&self, feed: Feed) -> Result<OpenCapture, String>;
}

pub struct OpenCapture {
    pub sample_rate: u32,
    /// Wait for one more capture callback at stop; the file source buffers nothing.
    pub drain_on_stop: bool,
    pub guard: Box<dyn Any>,
}

pub fn source_for(input: &AudioInput) -> Arc<dyn AudioSource> {
    match input {
        AudioInput::Default => Arc::new(CpalSource { device: None }),
        AudioInput::Device(name) => Arc::new(CpalSource { device: Some(name.clone()) }),
        AudioInput::File(path) => Arc::new(FileSource { path: path.clone() }),
    }
}

#[derive(Debug, Default)]
pub struct FeedShared {
    /// At the source's rate. Pushed before any newer samples; the recorder appends what's still
    /// owed once capture has stopped.
    owed_silence: AtomicU64,
    glitched: AtomicBool,
    callbacks: AtomicU64,
    failed: Mutex<Option<String>>,
}

impl FeedShared {
    /// Call only once capture has stopped.
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

    pub fn glitch(&self) {
        self.glitched.store(true, Ordering::Release);
    }

    pub fn fail(&self, reason: impl FnOnce() -> String) {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.get_or_insert_with(reason);
    }
}

/// Used from the capture callback, so it must never block.
pub struct Feed {
    producer: rtrb::Producer<f32>,
    shared: Arc<FeedShared>,
}

impl Feed {
    pub fn new(producer: rtrb::Producer<f32>, shared: Arc<FeedShared>) -> Feed {
        Feed { producer, shared }
    }

    /// What doesn't fit is replaced by silence, so the recording keeps its length and order.
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

    pub fn push_silence(&mut self, n: usize) {
        self.owe(n);
        self.pay_silence();
    }

    fn owe(&self, n: usize) {
        self.shared.owed_silence.fetch_add(n as u64, Ordering::AcqRel);
        self.shared.glitched.store(true, Ordering::Release);
    }

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

    pub fn glitch(&self) {
        self.shared.glitch();
    }

    pub fn tick(&self) {
        self.shared.callbacks.fetch_add(1, Ordering::AcqRel);
    }

    pub fn fail(&self, reason: String) {
        self.shared.fail(|| reason);
    }

    pub fn shared(&self) -> &Arc<FeedShared> {
        &self.shared
    }
}

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
        feed.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
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
