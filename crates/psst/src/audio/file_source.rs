use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use psst_proto::{FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};

use super::{AudioSource, Feed, OpenCapture};

pub struct FileSource {
    pub path: PathBuf,
}

struct Pacer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl AudioSource for FileSource {
    fn open(&self, mut feed: Feed) -> Result<OpenCapture, String> {
        let info = psst_client::read_wav_info(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let bytes = fs::read(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let start = info.data_offset as usize;
        let data = &bytes[start.min(bytes.len())..];
        let samples: Vec<f32> =
            data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("psst-file-audio".into())
            .spawn(move || {
                let started = Instant::now();
                let silence = [0.0f32; FRAME_SAMPLES];
                for chunk in 0u64.. {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    let pos = chunk as usize * FRAME_SAMPLES;
                    let piece = samples.get(pos..(pos + FRAME_SAMPLES).min(samples.len())).unwrap_or(&[]);
                    feed.push(piece);
                    if piece.len() < FRAME_SAMPLES {
                        feed.push(&silence[..FRAME_SAMPLES - piece.len()]);
                    }
                    feed.tick();
                    let next = started + Duration::from_millis((chunk + 1) * FRAME_MS as u64);
                    while let Some(wait) = next.checked_duration_since(Instant::now()) {
                        if thread_stop.load(Ordering::Acquire) {
                            return;
                        }
                        thread::park_timeout(wait);
                    }
                }
            })
            .map_err(|e| format!("cannot start the file audio thread: {e}"))?;
        Ok(OpenCapture {
            sample_rate: SAMPLE_RATE,
            drain_on_stop: false,
            guard: Box::new(Pacer { stop, thread: Some(thread) }),
        })
    }
}
