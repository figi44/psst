//! Capture discontinuities, found from the capture timestamps of consecutive buffers.
//!
//! Testing found a gap that came with no error from cpal at all, so an xrun isn't the only sign
//! of lost audio. A buffer that starts more than half a buffer after
//! the previous one ended is a discontinuity: the samples in between are missing, and are filled
//! with silence in their place.
//!
//! Each buffer is measured against the end of the one before it (its capture time plus its
//! frame count), never against the start of the recording. The device's clock and the system's
//! differ by a few hundredths of a percent, and that difference must not add up to a false gap
//! over a long recording.

use std::time::Duration;

use cpal::StreamInstant;

use super::Feed;

/// The longest discontinuity filled with silence. The one seen in testing was 32 ms; a jump of more than this
/// is the timestamps' clock jumping rather than audio lost, and filling it would put that much
/// silence into the middle of the recording. It still counts as a glitch.
pub const MAX_FILL: Duration = Duration::from_secs(1);

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// What came before a buffer instead of the end of the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discontinuity {
    /// This many samples are missing: they become silence.
    Missing(usize),
    /// The timestamps jumped forward by more than [`MAX_FILL`]: a glitch of unknown length.
    Jump(Duration),
}

/// Follows the capture timestamps of one stream.
#[derive(Debug)]
pub struct GapDetector {
    rate: u32,
    /// Where the previous buffer ended, and its length.
    previous: Option<(StreamInstant, Duration)>,
}

impl GapDetector {
    /// A detector for a stream of `rate` frames per second.
    pub fn new(rate: u32) -> GapDetector {
        GapDetector { rate: rate.max(1), previous: None }
    }

    /// Pushes a buffer captured at `capture` into `feed`, after silence for the samples missing
    /// before it. Never blocks.
    pub fn push(&mut self, feed: &mut Feed, capture: StreamInstant, samples: &[f32]) {
        match self.buffer(capture, samples.len()) {
            Some(Discontinuity::Missing(n)) => feed.push_silence(n),
            Some(Discontinuity::Jump(_)) => feed.glitch(),
            None => {}
        }
        feed.push(samples);
    }

    /// Takes the next buffer, `frames` long and captured at `capture`, and returns the
    /// discontinuity before it, if any.
    ///
    /// "Half a buffer" is half of the longer of this buffer and the previous one, so a quantum
    /// that shrinks (or a short first buffer) doesn't turn ordinary jitter into a gap. A buffer
    /// that starts before the previous one ended is jitter too.
    pub fn buffer(&mut self, capture: StreamInstant, frames: usize) -> Option<Discontinuity> {
        let length = Duration::from_nanos((frames as u128 * NANOS_PER_SEC / u128::from(self.rate)) as u64);
        let found = self.previous.and_then(|(end, previous_length)| {
            let gap = capture.checked_duration_since(end)?;
            if gap > MAX_FILL {
                Some(Discontinuity::Jump(gap))
            } else if gap > previous_length.max(length) / 2 {
                let samples = (gap.as_nanos() * u128::from(self.rate) + NANOS_PER_SEC / 2) / NANOS_PER_SEC;
                Some(Discontinuity::Missing(samples as usize))
            } else {
                None
            }
        });
        self.previous = capture.checked_add(length).map(|end| (end, length));
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FeedShared;

    /// PipeWire's default quantum, 1024 frames at 48 kHz, as the 16 kHz stream gets it.
    const QUANTUM: usize = 341;
    const RATE: u32 = 16_000;

    fn at_micros(micros: u64) -> StreamInstant {
        StreamInstant::from_micros(1_000_000 + micros)
    }

    /// Feeds buffers of `frames` captured at the given times (µs), returning what each found.
    fn run(buffers: &[(u64, usize)]) -> Vec<Option<Discontinuity>> {
        let mut gaps = GapDetector::new(RATE);
        buffers.iter().map(|&(micros, frames)| gaps.buffer(at_micros(micros), frames)).collect()
    }

    /// The start time (µs) of buffer `i` of a gapless stream of `frames`-frame buffers.
    fn start(i: u64, frames: usize) -> u64 {
        i * frames as u64 * 1_000_000 / u64::from(RATE)
    }

    #[test]
    fn buffers_that_follow_on_have_no_gaps_even_with_jitter() {
        let jitter = [0, 3_000, -2_000, 1_500, -3_000, 2_500, 0, -1_000];
        let buffers: Vec<(u64, usize)> = (0..200)
            .map(|i| ((start(i, QUANTUM) as i64 + jitter[i as usize % jitter.len()]) as u64, QUANTUM))
            .collect();
        assert!(run(&buffers).iter().all(Option::is_none));
    }

    #[test]
    fn a_buffer_that_starts_late_follows_the_samples_missing_before_it() {
        // The gap seen in testing: 32 ms lost between two buffers, with no xrun.
        let mut buffers: Vec<(u64, usize)> = (0..10).map(|i| (start(i, QUANTUM), QUANTUM)).collect();
        let resumed = start(10, QUANTUM) + 32_000;
        buffers.extend((0..10).map(|i| (resumed + start(i, QUANTUM), QUANTUM)));
        let found = run(&buffers);
        assert_eq!(found[10], Some(Discontinuity::Missing(512)));
        assert_eq!(found.iter().filter(|f| f.is_some()).count(), 1, "the buffers after it follow on: {found:?}");
    }

    #[test]
    fn up_to_half_a_buffer_late_is_jitter_and_more_is_a_gap() {
        // 320-frame buffers are 20 ms long.
        assert_eq!(run(&[(0, 320), (30_000, 320)])[1], None);
        assert_eq!(run(&[(0, 320), (30_063, 320)])[1], Some(Discontinuity::Missing(161)));
    }

    #[test]
    fn a_shorter_buffer_does_not_lower_the_threshold() {
        // The quantum drops to 58 frames (3.6 ms) and the buffer comes 5 ms late: still jitter
        // against the 21 ms buffer before it.
        let first_end = start(1, QUANTUM);
        assert_eq!(run(&[(0, QUANTUM), (first_end + 5_000, 58)])[1], None);
        // A short first buffer, then a full one 5 ms late.
        assert_eq!(run(&[(0, 58), (start(1, 58) + 5_000, QUANTUM)])[1], None);
    }

    #[test]
    fn the_clocks_drifting_apart_never_adds_up_to_a_gap() {
        // Each buffer starts 0.05% later than its predecessor's frame count says: 53 ms over
        // 5,000 buffers (107 s), but never more than a few microseconds between two of them.
        let buffers: Vec<(u64, usize)> = (0..5_000).map(|i| (start(i, QUANTUM) * 10_005 / 10_000, QUANTUM)).collect();
        assert!(start(5_000, QUANTUM) * 5 / 10_000 > 50_000);
        assert!(run(&buffers).iter().all(Option::is_none));
    }

    #[test]
    fn a_buffer_that_starts_early_is_measured_from_where_it_ends() {
        // The second buffer overlaps the first by 5 ms; the third follows the second.
        let found = run(&[(0, 320), (15_000, 320), (35_000, 320), (55_000, 320)]);
        assert_eq!(found, [None, None, None, None]);
        // And a gap after an early buffer is measured from the early buffer's end.
        assert_eq!(run(&[(0, 320), (15_000, 320), (35_000 + 64_000, 320)])[2], Some(Discontinuity::Missing(1024)));
    }

    #[test]
    fn a_jump_of_the_clock_is_a_glitch_and_not_silence() {
        let found = run(&[(0, 320), (20_000 + 5_000_000, 320), (20_000 + 5_020_000, 320)]);
        assert_eq!(found, [None, Some(Discontinuity::Jump(Duration::from_secs(5))), None]);
        // Exactly MAX_FILL is still filled.
        assert_eq!(run(&[(0, 320), (1_020_000, 320)])[1], Some(Discontinuity::Missing(16_000)));
    }

    #[test]
    fn the_silence_goes_into_the_feed_before_the_late_buffer() {
        let (producer, mut ring) = rtrb::RingBuffer::new(4_096);
        let shared = std::sync::Arc::new(FeedShared::default());
        let mut feed = Feed::new(producer, shared.clone());
        let mut gaps = GapDetector::new(RATE);
        gaps.push(&mut feed, at_micros(0), &[0.5; 320]);
        assert!(!shared.take_glitched());
        gaps.push(&mut feed, at_micros(20_000 + 2_000), &[0.25; 320]); // jitter
        gaps.push(&mut feed, at_micros(42_000 + 64_000), &[0.75; 320]); // 64 ms lost
        let mut out = vec![0.0; ring.slots()];
        let (popped, _) = ring.pop_partial_slice(&mut out);
        let expected: Vec<f32> = [vec![0.5; 320], vec![0.25; 320], vec![0.0; 1_024], vec![0.75; 320]].concat();
        assert_eq!(popped, &expected[..]);
        assert!(shared.take_glitched(), "the recording is gapped");
        assert_eq!(shared.take_owed_silence(), 0);
    }
}
