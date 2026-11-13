//! The error sound (plan 9.3): two short falling tones on the default output device, played on a
//! thread of its own so nothing waits for it. `sounds = false` turns it off.

use std::{
    f32::consts::TAU,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use cpal::{
    SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tracing::debug;

/// Plays the error sound unless one is already playing.
pub fn play_error() {
    static PLAYING: AtomicUsize = AtomicUsize::new(0);
    if PLAYING.fetch_add(1, Ordering::AcqRel) > 0 {
        PLAYING.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let spawned = thread::Builder::new().name("psst-sound".into()).spawn(|| {
        if let Err(err) = play() {
            debug!("cannot play the error sound: {err}");
        }
        PLAYING.fetch_sub(1, Ordering::AcqRel);
    });
    if spawned.is_err() {
        PLAYING.fetch_sub(1, Ordering::AcqRel);
    }
}

fn play() -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let stream_config = config.config();
    let samples = Arc::new(tone(stream_config.sample_rate));
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, stream_config, samples.clone(), |s| s),
        SampleFormat::I16 => build::<i16>(&device, stream_config, samples.clone(), |s| (s * 32767.0) as i16),
        other => return Err(format!("unsupported output format {other}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    let length = samples.len() as f32 / stream_config.sample_rate as f32;
    thread::sleep(Duration::from_secs_f32(length) + Duration::from_millis(150));
    Ok(())
}

fn build<T: SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: StreamConfig,
    samples: Arc<Vec<f32>>,
    convert: fn(f32) -> T,
) -> Result<cpal::Stream, String> {
    let channels = config.channels.max(1) as usize;
    let mut pos = 0;
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| {
                for frame in out.chunks_mut(channels) {
                    let value = convert(samples.get(pos).copied().unwrap_or(0.0));
                    frame.fill(value);
                    pos += 1;
                }
            },
            |err| debug!("error sound: {err}"),
            Some(Duration::from_secs(1)),
        )
        .map_err(|e| e.to_string())
}

/// 660 Hz then 440 Hz, 120 ms each with short fades.
fn tone(rate: u32) -> Vec<f32> {
    let mut out = Vec::new();
    for freq in [660.0f32, 440.0] {
        let n = rate as usize * 120 / 1000;
        let fade = (rate as usize * 10 / 1000).max(1);
        for i in 0..n {
            let envelope = (i.min(n - i) as f32 / fade as f32).min(1.0);
            out.push((i as f32 * freq * TAU / rate as f32).sin() * 0.25 * envelope);
        }
        out.extend(std::iter::repeat_n(0.0, rate as usize * 40 / 1000));
    }
    out
}
