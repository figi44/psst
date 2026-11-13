//! Capture through cpal, with its native PipeWire backend when PipeWire is running (plan 9.3).
//!
//! It asks for 16 kHz mono `f32`; if the device can't provide that, it captures the device's
//! default format and downmixes in the callback, and the recorder resamples.

use std::time::Duration;

use cpal::{
    ErrorKind, InputCallbackInfo, SampleFormat, SizedSample, StreamConfig, StreamInstant,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use psst_proto::SAMPLE_RATE;
use tracing::{debug, warn};

use super::{AudioSource, Feed, OpenCapture};

/// A capture discontinuity shorter than this is jitter, not a gap.
const GAP_TOLERANCE: Duration = Duration::from_millis(10);

pub struct CpalSource {
    /// A capture device's name; the system default when `None`.
    pub device: Option<String>,
}

impl AudioSource for CpalSource {
    fn open(&self, feed: Feed) -> Result<OpenCapture, String> {
        let host = cpal::default_host();
        let device = match &self.device {
            None => host.default_input_device().ok_or("no default capture device")?,
            Some(name) => host
                .input_devices()
                .map_err(|e| format!("cannot list capture devices: {e}"))?
                .find(|d| d.description().is_ok_and(|desc| desc.name() == name) || d.to_string() == *name)
                .ok_or_else(|| format!("no capture device named `{name}`"))?,
        };
        debug!("capturing from {device} on {:?}", host.id());
        let wanted = StreamConfig { channels: 1, sample_rate: SAMPLE_RATE, buffer_size: cpal::BufferSize::Default };
        let mut feed = Some(feed);
        match build::<f32>(&device, wanted, &mut feed) {
            Ok(stream) => return play(stream, SAMPLE_RATE),
            Err(err) if err.kind() == ErrorKind::UnsupportedConfig => {
                debug!("16 kHz mono isn't available ({err}); capturing the default format");
            }
            Err(err) => return Err(format!("cannot open {device}: {err}")),
        }
        let config = device.default_input_config().map_err(|e| format!("{device}: {e}"))?;
        let stream_config = config.config();
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, stream_config, &mut feed),
            SampleFormat::I16 => build::<i16>(&device, stream_config, &mut feed),
            SampleFormat::I32 => build::<i32>(&device, stream_config, &mut feed),
            SampleFormat::U16 => build::<u16>(&device, stream_config, &mut feed),
            other => return Err(format!("{device}: unsupported sample format {other}")),
        }
        .map_err(|e| format!("cannot open {device}: {e}"))?;
        play(stream, stream_config.sample_rate)
    }
}

fn play(stream: cpal::Stream, sample_rate: u32) -> Result<OpenCapture, String> {
    stream.play().map_err(|e| format!("cannot start capture: {e}"))?;
    Ok(OpenCapture { sample_rate, drain_on_stop: true, guard: Box::new(stream) })
}

/// Samples cpal can hand us, converted to `f32` in −1…1.
trait ToF32: SizedSample + Send + 'static {
    fn to_f32(self) -> f32;
}

impl ToF32 for f32 {
    fn to_f32(self) -> f32 {
        self
    }
}

impl ToF32 for i16 {
    fn to_f32(self) -> f32 {
        self as f32 / 32768.0
    }
}

impl ToF32 for i32 {
    fn to_f32(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }
}

impl ToF32 for u16 {
    fn to_f32(self) -> f32 {
        (self as f32 - 32768.0) / 32768.0
    }
}

/// Builds the stream, moving `feed` into its callbacks only on success.
fn build<T: ToF32>(
    device: &cpal::Device,
    config: StreamConfig,
    feed: &mut Option<Feed>,
) -> Result<cpal::Stream, cpal::Error> {
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate as f64;
    // The callbacks take the feed only once the stream exists; until then it waits here.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<Feed>));
    let error_shared = feed.as_ref().expect("a feed to build with").shared().clone();
    let data_slot = slot.clone();
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let mut expected: Option<StreamInstant> = None;
    let mut taken: Option<Feed> = None;
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], info: &InputCallbackInfo| {
            if taken.is_none() {
                taken = data_slot.try_lock().ok().and_then(|mut slot| slot.take());
            }
            let Some(feed) = taken.as_mut() else { return };
            let frames = data.len() / channels;
            // A capture timestamp past where this buffer should start is a discontinuity: fill it.
            let capture = info.timestamp().capture;
            if let Some(expected) = expected
                && let Some(gap) = capture.checked_duration_since(expected)
                && gap > GAP_TOLERANCE
            {
                feed.push_silence((gap.as_secs_f64() * rate).round() as usize);
            }
            expected = capture.checked_add(Duration::from_secs_f64(frames as f64 / rate));
            mono.clear();
            for frame in data.chunks_exact(channels) {
                mono.push(frame.iter().map(|s| s.to_f32()).sum::<f32>() / channels as f32);
            }
            feed.push(&mono);
            feed.tick();
        },
        move |err: cpal::Error| match err.kind() {
            ErrorKind::Xrun => {
                error_shared.glitched.store(true, std::sync::atomic::Ordering::Release);
            }
            ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied => debug!("capture: {err}"),
            ErrorKind::DeviceNotAvailable
            | ErrorKind::StreamInvalidated
            | ErrorKind::HostUnavailable
            | ErrorKind::PermissionDenied => {
                warn!("capture failed: {err}");
                let mut failed = error_shared.failed.lock().unwrap_or_else(|e| e.into_inner());
                failed.get_or_insert_with(|| format!("the capture device is gone: {err}"));
            }
            _ => warn!("capture: {err}"),
        },
        Some(Duration::from_secs(2)),
    )?;
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = feed.take();
    Ok(stream)
}
