//! The recorder thread: drains the capture ring, resamples to 16 kHz, writes the WAV from the
//! first frame, and hands frames to the daemon.

use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use psst_client::AudioFrame;
use psst_proto::{FRAME_SAMPLES, SAMPLE_RATE};
use rubato::{Fft, FixedSync, Indexing, Resampler, audioadapter_buffers::direct::InterleavedSlice};

use super::{AudioSource, Feed, FeedShared, RING_SAMPLES, level};

/// How often the recorder drains the ring while recording.
const POLL: Duration = Duration::from_millis(10);
/// Longest wait at stop for the capture callback that holds the last audio.
const DRAIN_WAIT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq)]
pub enum RecEvent {
    /// A 20 ms frame, already written to the WAV, with its level (0…1). The last frame of a
    /// recording is padded with zeros.
    Frame { frame: Box<AudioFrame>, level: f32 },
    /// A glitch was filled with silence; the recording is `gapped`.
    Gap,
    /// Recording stopped for good: the file couldn't be written or the device is gone.
    /// `samples` real samples are in the WAV.
    Failed { reason: String, samples: u64 },
    /// Stopped as asked; every frame was sent before this. `samples` real samples are in the WAV.
    Finished { samples: u64, gapped: bool },
}

/// Controls a running recorder.
#[derive(Debug)]
pub struct Recorder {
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    thread: thread::Thread,
}

impl Recorder {
    /// Stops after draining the capture and flushing the resampler, then sends `Finished`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.thread.unpark();
    }

    /// Stops at once without further events (for `cancel`).
    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Creates `wav` and starts recording into it on a new thread. Events go to `emit`, in order.
/// The file exists when this returns, so deleting it afterwards always works.
pub fn start(
    source: Arc<dyn AudioSource>,
    wav: PathBuf,
    emit: impl Fn(RecEvent) + Send + 'static,
) -> io::Result<Recorder> {
    let writer =
        WavWriter::create(&wav).map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", wav.display())))?;
    let stop = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (thread_stop, thread_abort) = (stop.clone(), abort.clone());
    let handle = thread::Builder::new()
        .name("psst-recorder".into())
        .spawn(move || run(source, writer, &thread_stop, &thread_abort, &emit))?;
    Ok(Recorder { stop, abort, thread: handle.thread().clone() })
}

fn run(
    source: Arc<dyn AudioSource>,
    mut wav: WavWriter,
    stop: &AtomicBool,
    abort: &AtomicBool,
    emit: &dyn Fn(RecEvent),
) {
    let (producer, mut ring) = rtrb::RingBuffer::new(RING_SAMPLES);
    let shared = Arc::new(FeedShared::default());
    let capture = match source.open(Feed::new(producer, shared.clone())) {
        Ok(capture) => capture,
        Err(reason) => {
            emit(RecEvent::Failed { reason, samples: 0 });
            return;
        }
    };
    let mut conv = match Converter::new(capture.sample_rate) {
        Ok(conv) => conv,
        Err(reason) => {
            emit(RecEvent::Failed { reason, samples: 0 });
            return;
        }
    };
    let mut gapped = false;
    let mut input = vec![0.0f32; 8192];
    let mut frames = Vec::new();

    // Moves what the ring holds through the converter and out as frames. `Err` ends recording.
    let mut pump = |ring: &mut rtrb::Consumer<f32>,
                    conv: &mut Converter,
                    wav: &mut WavWriter,
                    gapped: &mut bool,
                    frames: &mut Vec<i16>| {
        if shared.take_glitched() {
            *gapped = true;
            emit(RecEvent::Gap);
        }
        loop {
            let (got, _) = ring.pop_partial_slice(&mut input);
            let n = got.len();
            if n == 0 {
                break;
            }
            conv.push(&input[..n], frames);
        }
        send_frames(frames, wav, emit)
    };

    let mut stopping_since: Option<(Instant, u64)> = None;
    loop {
        if abort.load(Ordering::Acquire) {
            return;
        }
        if let Some(reason) = shared.failure() {
            drop(capture);
            if pump(&mut ring, &mut conv, &mut wav, &mut gapped, &mut frames).is_ok() {
                conv.push_silence(shared.take_owed_silence() as usize, &mut frames);
                let _ = wav.write(&frames);
            }
            let _ = wav.finish();
            emit(RecEvent::Failed { reason, samples: wav.samples });
            return;
        }
        if stop.load(Ordering::Acquire) {
            let (since, callbacks) = *stopping_since.get_or_insert((Instant::now(), shared.callbacks()));
            let drained = !capture.drain_on_stop || shared.callbacks() > callbacks || since.elapsed() >= DRAIN_WAIT;
            if drained {
                break;
            }
        }
        if let Err(reason) = pump(&mut ring, &mut conv, &mut wav, &mut gapped, &mut frames) {
            drop(capture);
            emit(RecEvent::Failed { reason, samples: wav.samples });
            return;
        }
        thread::park_timeout(if stopping_since.is_some() { Duration::from_millis(2) } else { POLL });
    }
    drop(capture);
    if let Err(reason) = pump(&mut ring, &mut conv, &mut wav, &mut gapped, &mut frames) {
        emit(RecEvent::Failed { reason, samples: wav.samples });
        return;
    }
    // Silence for samples that never fit in the ring belongs after everything in it.
    conv.push_silence(shared.take_owed_silence() as usize, &mut frames);
    conv.finish(&mut frames);
    if let Err(reason) = send_frames(&mut frames, &mut wav, emit) {
        emit(RecEvent::Failed { reason, samples: wav.samples });
        return;
    }
    // What's left is less than a frame: written as is, sent padded.
    let last = frames;
    let real = last.len();
    if real > 0 {
        if let Err(err) = wav.write(&last) {
            emit(RecEvent::Failed { reason: format!("cannot write the recording: {err}"), samples: wav.samples });
            return;
        }
        let mut frame = [0i16; FRAME_SAMPLES];
        frame[..real].copy_from_slice(&last);
        emit(RecEvent::Frame { frame: Box::new(frame), level: level(&last) });
    }
    if let Err(err) = wav.finish() {
        emit(RecEvent::Failed { reason: format!("cannot write the recording: {err}"), samples: wav.samples });
        return;
    }
    emit(RecEvent::Finished { samples: wav.samples, gapped });
}

/// Writes every whole frame in `pending` and emits it; keeps the remainder.
fn send_frames(pending: &mut Vec<i16>, wav: &mut WavWriter, emit: &dyn Fn(RecEvent)) -> Result<(), String> {
    let whole = pending.len() / FRAME_SAMPLES * FRAME_SAMPLES;
    for frame in pending[..whole].as_chunks::<FRAME_SAMPLES>().0 {
        wav.write(frame).map_err(|err| format!("cannot write the recording: {err}"))?;
        emit(RecEvent::Frame { frame: Box::new(*frame), level: level(frame) });
    }
    pending.drain(..whole);
    Ok(())
}

/// Resamples mono `f32` at the capture rate to 16 kHz `i16`.
struct Converter {
    resampler: Option<Fft<f32>>,
    rate: u32,
    /// Input waiting for a whole resampler chunk.
    input: Vec<f32>,
    /// Output samples still to skip: the resampler's delay.
    skip: usize,
    in_total: u64,
    out_total: u64,
    out: Vec<f32>,
}

impl Converter {
    fn new(rate: u32) -> Result<Converter, String> {
        let resampler = if rate == SAMPLE_RATE {
            None
        } else {
            let chunk = (rate / 100).max(1) as usize; // 10 ms
            Some(
                Fft::<f32>::new(rate as usize, SAMPLE_RATE as usize, chunk, 1, FixedSync::Input)
                    .map_err(|e| format!("cannot resample from {rate} Hz: {e}"))?,
            )
        };
        let skip = resampler.as_ref().map_or(0, |r| r.output_delay());
        let out = vec![0.0; resampler.as_ref().map_or(0, |r| r.output_frames_max())];
        Ok(Converter { resampler, rate, input: Vec::new(), skip, in_total: 0, out_total: 0, out })
    }

    fn push_silence(&mut self, n: usize, frames: &mut Vec<i16>) {
        let zeros = vec![0.0; n];
        self.push(&zeros, frames);
    }

    fn push(&mut self, samples: &[f32], frames: &mut Vec<i16>) {
        self.in_total += samples.len() as u64;
        let Some(resampler) = &mut self.resampler else {
            self.out_total += samples.len() as u64;
            frames.extend(samples.iter().map(|&s| to_i16(s)));
            return;
        };
        self.input.extend_from_slice(samples);
        loop {
            let need = resampler.input_frames_next();
            if self.input.len() < need {
                break;
            }
            Self::process(
                resampler,
                &self.input[..need],
                None,
                &mut self.out,
                &mut self.skip,
                &mut self.out_total,
                frames,
            );
            self.input.drain(..need);
        }
    }

    /// Flushes the resampler so every input sample has its output, trimmed to the exact length.
    fn finish(&mut self, frames: &mut Vec<i16>) {
        let expected = (self.in_total as u128 * SAMPLE_RATE as u128 / self.rate as u128) as u64;
        let Some(resampler) = &mut self.resampler else { return };
        let mut pending = std::mem::take(&mut self.input);
        let mut produced = Vec::new();
        while self.out_total < expected {
            let need = resampler.input_frames_next();
            let partial = pending.len().min(need);
            let mut chunk = vec![0.0; need];
            chunk[..partial].copy_from_slice(&pending[..partial]);
            pending.drain(..partial);
            Self::process(resampler, &chunk, None, &mut self.out, &mut self.skip, &mut self.out_total, &mut produced);
        }
        let excess = (self.out_total - expected) as usize;
        produced.truncate(produced.len() - excess.min(produced.len()));
        self.out_total = expected;
        frames.extend(produced);
    }

    fn process(
        resampler: &mut Fft<f32>,
        input: &[f32],
        indexing: Option<&Indexing>,
        out: &mut [f32],
        skip: &mut usize,
        out_total: &mut u64,
        frames: &mut Vec<i16>,
    ) {
        let frames_in = input.len();
        let frames_out = out.len();
        let adapter_in = InterleavedSlice::new(input, 1, frames_in).expect("mono input");
        let mut adapter_out = InterleavedSlice::new_mut(out, 1, frames_out).expect("mono output");
        let (_, written) = resampler
            .process_into_buffer(&adapter_in, &mut adapter_out, indexing)
            .expect("buffers are sized by the resampler");
        let start = (*skip).min(written);
        *skip -= start;
        *out_total += (written - start) as u64;
        frames.extend(out[start..written].iter().map(|&s| to_i16(s)));
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// A 16 kHz mono s16 WAV written as audio arrives. The header's sizes are filled in at the end;
/// readers take the length from the file size anyway.
struct WavWriter {
    file: File,
    samples: u64,
}

impl WavWriter {
    fn create(path: &PathBuf) -> io::Result<WavWriter> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        file.write_all(&header(0))?;
        Ok(WavWriter { file, samples: 0 })
    }

    fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        let data_len = u32::try_from(self.samples * 2).unwrap_or(u32::MAX);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(data_len))?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

fn header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data_len.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, mpsc};

    use crate::audio::{OpenCapture, gaps::GapDetector};

    use super::*;

    /// A source that pushes a fixed signal at `rate` in one go, optionally failing afterwards.
    struct Scripted {
        rate: u32,
        samples: Vec<f32>,
        fail: Option<String>,
    }

    impl AudioSource for Scripted {
        fn open(&self, mut feed: Feed) -> Result<OpenCapture, String> {
            feed.push(&self.samples);
            if let Some(reason) = &self.fail {
                feed.fail(reason.clone());
            }
            feed.tick();
            Ok(OpenCapture { sample_rate: self.rate, drain_on_stop: false, guard: Box::new(Mutex::new(feed)) })
        }
    }

    fn record(source: impl AudioSource, run_for: Duration) -> (Vec<RecEvent>, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("rec.wav");
        let (tx, rx) = mpsc::channel();
        let recorder = start(Arc::new(source), wav.clone(), move |e| tx.send(e).unwrap()).unwrap();
        thread::sleep(run_for);
        recorder.stop();
        let mut events = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(5)) {
            let done = matches!(event, RecEvent::Finished { .. } | RecEvent::Failed { .. });
            events.push(event);
            if done {
                break;
            }
        }
        (events, wav, dir)
    }

    fn audio_of(events: &[RecEvent]) -> Vec<i16> {
        events
            .iter()
            .filter_map(|e| if let RecEvent::Frame { frame, .. } = e { Some(frame.to_vec()) } else { None })
            .flatten()
            .collect()
    }

    #[test]
    fn sixteen_khz_input_is_written_and_framed_unchanged() {
        let signal: Vec<f32> = (0..16_100).map(|i| ((i % 200) as f32 - 100.0) / 200.0).collect();
        let (events, wav, _dir) =
            record(Scripted { rate: 16_000, samples: signal.clone(), fail: None }, Duration::from_millis(50));
        assert_eq!(events.last(), Some(&RecEvent::Finished { samples: 16_100, gapped: false }));
        let expected: Vec<i16> = signal.iter().map(|&s| to_i16(s)).collect();
        let sent = audio_of(&events);
        assert_eq!(sent.len(), 51 * FRAME_SAMPLES, "the last frame is padded");
        assert_eq!(&sent[..16_100], &expected[..]);
        assert!(sent[16_100..].iter().all(|&s| s == 0));
        let info = psst_client::read_wav_info(&wav).unwrap();
        assert_eq!(info.samples, 16_100);
        let bytes = std::fs::read(&wav).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 32_200, "the header is finalised");
    }

    #[test]
    fn other_rates_are_resampled_to_the_exact_length_with_the_tail_kept() {
        // One second of a 440 Hz tone at 48 kHz, loud until the very end.
        let signal: Vec<f32> =
            (0..48_000).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5).collect();
        let (events, _wav, _dir) =
            record(Scripted { rate: 48_000, samples: signal, fail: None }, Duration::from_millis(50));
        assert_eq!(events.last(), Some(&RecEvent::Finished { samples: 16_000, gapped: false }));
        let sent = audio_of(&events);
        // The tone reaches the last 10 ms instead of being cut off with the resampler's delay.
        let tail = &sent[16_000 - 160..16_000];
        assert!(level(tail) > 0.7, "tail level {}", level(tail));
        let head = &sent[..160];
        assert!(level(head) > 0.7, "no leading delay: head level {}", level(head));
    }

    #[test]
    fn overflow_becomes_silence_after_the_audio_before_it() {
        // A full ring, then 320 samples that don't fit.
        let mut signal = vec![0.25; RING_SAMPLES];
        signal.extend([0.5; 320]);
        let (events, wav, _dir) =
            record(Scripted { rate: 16_000, samples: signal, fail: None }, Duration::from_millis(50));
        assert!(events.contains(&RecEvent::Gap));
        let total = RING_SAMPLES as u64 + 320;
        assert_eq!(events.last(), Some(&RecEvent::Finished { samples: total, gapped: true }));
        let sent = audio_of(&events);
        assert!(sent[..RING_SAMPLES].iter().all(|&s| s == to_i16(0.25)));
        assert!(sent[RING_SAMPLES..].iter().all(|&s| s == 0), "the gap is where the lost audio was");
        assert_eq!(psst_client::read_wav_info(&wav).unwrap().samples, total);
    }

    /// A source that delivers 16 kHz buffers captured at the given times (µs), as cpal does.
    struct Timestamped(Vec<(u64, Vec<f32>)>);

    impl AudioSource for Timestamped {
        fn open(&self, mut feed: Feed) -> Result<OpenCapture, String> {
            let mut gaps = GapDetector::new(16_000);
            for (micros, samples) in &self.0 {
                gaps.push(&mut feed, cpal::StreamInstant::from_micros(*micros), samples);
                feed.tick();
            }
            Ok(OpenCapture { sample_rate: 16_000, drain_on_stop: false, guard: Box::new(Mutex::new(feed)) })
        }
    }

    #[test]
    fn a_gap_in_the_capture_timestamps_becomes_silence_and_the_recording_is_gapped() {
        // Two 20 ms buffers, then one that starts 64 ms after the second ended, without an xrun.
        let source = Timestamped(vec![(0, vec![0.25; 320]), (20_000, vec![0.25; 320]), (104_000, vec![0.5; 320])]);
        let (events, wav, _dir) = record(source, Duration::from_millis(50));
        assert!(events.contains(&RecEvent::Gap));
        assert_eq!(events.last(), Some(&RecEvent::Finished { samples: 1_984, gapped: true }));
        let expected: Vec<i16> = [vec![to_i16(0.25); 640], vec![0; 1_024], vec![to_i16(0.5); 320]].concat();
        assert_eq!(&audio_of(&events)[..1_984], &expected[..], "the silence is where the lost audio was");
        assert_eq!(psst_client::read_wav_info(&wav).unwrap().samples, 1_984);
    }

    #[test]
    fn a_lost_device_stops_the_recording_and_keeps_what_was_written() {
        let (events, wav, _dir) = record(
            Scripted { rate: 16_000, samples: vec![0.25; 3_200], fail: Some("unplugged".into()) },
            Duration::from_millis(50),
        );
        assert_eq!(events.last(), Some(&RecEvent::Failed { reason: "unplugged".into(), samples: 3_200 }));
        assert_eq!(psst_client::read_wav_info(&wav).unwrap().samples, 3_200);
    }

    #[test]
    fn a_file_that_cannot_be_created_fails_before_any_audio() {
        let source = Arc::new(Scripted { rate: 16_000, samples: vec![], fail: None });
        assert!(start(source, PathBuf::from("/nonexistent/dir/rec.wav"), |_| {}).is_err());
    }

    #[test]
    fn a_device_that_cannot_open_fails_before_any_audio() {
        struct Broken;
        impl AudioSource for Broken {
            fn open(&self, _: Feed) -> Result<OpenCapture, String> {
                Err("no such device".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        start(Arc::new(Broken), dir.path().join("rec.wav"), move |e| tx.send(e).unwrap()).unwrap();
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(event, RecEvent::Failed { reason: "no such device".into(), samples: 0 });
    }
}
