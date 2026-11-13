use std::time::Duration;

use cpal::StreamInstant;

use super::Feed;

/// A longer jump is the clock jumping, not lost audio (the real gap seen in testing was 32 ms), so
/// it's only a glitch and isn't filled with silence.
pub const MAX_FILL: Duration = Duration::from_secs(1);

const NANOS_PER_SEC: u128 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discontinuity {
    Missing(usize),
    Jump(Duration),
}

/// cpal can lose audio without reporting an xrun (seen in testing), so gaps are also found from the
/// capture timestamps.
///
/// Each buffer is measured against the end of the previous one, never the recording's start: the
/// device and system clocks differ by a few hundredths of a percent, which would add up to false
/// gaps over a long recording.
#[derive(Debug)]
pub struct GapDetector {
    rate: u32,
    /// The previous buffer's end and length.
    previous: Option<(StreamInstant, Duration)>,
}

impl GapDetector {
    pub fn new(rate: u32) -> GapDetector {
        GapDetector { rate: rate.max(1), previous: None }
    }

    pub fn push(&mut self, feed: &mut Feed, capture: StreamInstant, samples: &[f32]) {
        match self.buffer(capture, samples.len()) {
            Some(Discontinuity::Missing(n)) => feed.push_silence(n),
            Some(Discontinuity::Jump(_)) => feed.glitch(),
            None => {}
        }
        feed.push(samples);
    }

    /// Jitter is up to half the longer of this buffer and the previous one, so a shrinking quantum
    /// or a short first buffer doesn't turn jitter into a gap.
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

    /// PipeWire's default quantum (1024 frames at 48 kHz) at 16 kHz.
    const QUANTUM: usize = 341;
    const RATE: u32 = 16_000;

    fn at_micros(micros: u64) -> StreamInstant {
        StreamInstant::from_micros(1_000_000 + micros)
    }

    fn run(buffers: &[(u64, usize)]) -> Vec<Option<Discontinuity>> {
        let mut gaps = GapDetector::new(RATE);
        buffers.iter().map(|&(micros, frames)| gaps.buffer(at_micros(micros), frames)).collect()
    }

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
        // The gap seen in testing: 32 ms lost with no xrun.
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
        // A 3.6 ms buffer 5 ms late is still jitter against the 21 ms one before it.
        let first_end = start(1, QUANTUM);
        assert_eq!(run(&[(0, QUANTUM), (first_end + 5_000, 58)])[1], None);
        assert_eq!(run(&[(0, 58), (start(1, 58) + 5_000, QUANTUM)])[1], None);
    }

    #[test]
    fn the_clocks_drifting_apart_never_adds_up_to_a_gap() {
        // 0.05% drift: 53 ms over 107 s, but only microseconds between two buffers.
        let buffers: Vec<(u64, usize)> = (0..5_000).map(|i| (start(i, QUANTUM) * 10_005 / 10_000, QUANTUM)).collect();
        assert!(start(5_000, QUANTUM) * 5 / 10_000 > 50_000);
        assert!(run(&buffers).iter().all(Option::is_none));
    }

    #[test]
    fn a_buffer_that_starts_early_is_measured_from_where_it_ends() {
        // The second buffer overlaps the first by 5 ms.
        let found = run(&[(0, 320), (15_000, 320), (35_000, 320), (55_000, 320)]);
        assert_eq!(found, [None, None, None, None]);
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
