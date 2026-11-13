use std::time::Duration;

use cpal::{
    ErrorKind, InputCallbackInfo, SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use psst_proto::SAMPLE_RATE;
use tracing::{debug, info, warn};

use super::{AudioSource, Feed, FeedShared, OpenCapture, gaps::GapDetector};

pub struct CpalSource {
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
                .find(|d| {
                    let id = d.id().map(|id| id.to_string()).unwrap_or_default();
                    d.description().is_ok_and(|desc| desc.name() == name)
                        || d.to_string() == *name
                        || id == *name
                        || id.split_once(':').is_some_and(|(_, node)| node == name)
                })
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

/// Takes `feed` only on success, so a failed build can be retried with another config.
fn build<T: ToF32>(
    device: &cpal::Device,
    config: StreamConfig,
    feed: &mut Option<Feed>,
) -> Result<cpal::Stream, cpal::Error> {
    let channels = config.channels.max(1) as usize;
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<Feed>));
    let error_shared = feed.as_ref().expect("a feed to build with").shared().clone();
    let data_slot = slot.clone();
    let mut mono: Vec<f32> = Vec::with_capacity(8192);
    let mut gaps = GapDetector::new(config.sample_rate);
    let mut taken: Option<Feed> = None;
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], info: &InputCallbackInfo| {
            if taken.is_none() {
                taken = data_slot.try_lock().ok().and_then(|mut slot| slot.take());
            }
            let Some(feed) = taken.as_mut() else { return };
            mono.clear();
            for frame in data.chunks_exact(channels) {
                mono.push(frame.iter().map(|s| s.to_f32()).sum::<f32>() / channels as f32);
            }
            gaps.push(feed, info.timestamp().capture, &mono);
            feed.tick();
        },
        move |err: cpal::Error| on_stream_error(&error_shared, &err),
        Some(Duration::from_secs(2)),
    )?;
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = feed.take();
    Ok(stream)
}

fn on_stream_error(shared: &FeedShared, err: &cpal::Error) {
    match err.kind() {
        ErrorKind::Xrun => shared.glitch(),
        // RTKit refused, or its burst limit was hit.
        ErrorKind::RealtimeDenied => info!("capturing at normal priority: {err}"),
        ErrorKind::DeviceChanged => debug!("capture: {err}"),
        ErrorKind::DeviceNotAvailable
        | ErrorKind::StreamInvalidated
        | ErrorKind::HostUnavailable
        | ErrorKind::PermissionDenied => {
            warn!("capture failed: {err}");
            shared.fail(|| format!("the capture device is gone: {err}"));
        }
        _ => warn!("capture: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_real_time_priority_leaves_the_recording_alone() {
        let shared = FeedShared::default();
        let refused = cpal::Error::with_message(
            ErrorKind::RealtimeDenied,
            "Failed to promote audio thread to real-time priority: Device or resource busy",
        );
        on_stream_error(&shared, &refused);
        assert_eq!(shared.failure(), None);
        assert!(!shared.take_glitched());
    }

    #[test]
    fn an_xrun_is_a_glitch_and_a_lost_device_stops_the_recording() {
        let shared = FeedShared::default();
        on_stream_error(&shared, &ErrorKind::Xrun.into());
        assert!(shared.take_glitched());
        assert_eq!(shared.failure(), None);
        on_stream_error(&shared, &cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "unplugged"));
        assert!(shared.failure().is_some_and(|reason| reason.contains("unplugged")), "{:?}", shared.failure());
    }
}
