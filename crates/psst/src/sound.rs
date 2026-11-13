use std::{
    f64::consts::TAU,
    sync::{Arc, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use cpal::{
    SampleFormat, SizedSample, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use psst_config::Sounds;
use tracing::debug;

use crate::daemon::Sound;

const VOICES: usize = 8;
/// Past a burst's last sample, for the device's latency, before its stream closes.
const TAIL: Duration = Duration::from_millis(150);

/// The sound thread starts with the first sound.
pub fn player() -> impl Fn(Sounds, Sound) + Send + Sync + 'static {
    let queue: OnceLock<Option<mpsc::Sender<(Sounds, Sound)>>> = OnceLock::new();
    move |sounds, sound| {
        let tx = queue.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let spawned = thread::Builder::new().name("psst-sound".into()).spawn(move || {
                while let Ok(first) = rx.recv() {
                    if let Err(err) = burst(first, &rx) {
                        debug!("cannot play a sound: {err}");
                    }
                }
            });
            spawned.inspect_err(|err| debug!("cannot start the sound thread: {err}")).ok().map(|_| tx)
        });
        if let Some(tx) = tx {
            let _ = tx.send((sounds, sound));
        }
    }
}

/// Plays `first`, and whatever comes while it plays, on one stream, so a stop chime overlaps the
/// start chime's ring instead of waiting for it. The stream closes after, so the device can sleep.
fn burst(first: (Sounds, Sound), rx: &mpsc::Receiver<(Sounds, Sound)>) -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let stream_config = config.config();
    let (producer, consumer) = rtrb::RingBuffer::new(VOICES);
    // Declared before the stream, so the stream stops before the samples it plays are freed.
    let rate = stream_config.sample_rate;
    let mut voices = Voices { producer, rate, held: Vec::new(), ends: Instant::now(), last: None };
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, stream_config, consumer, |s| s),
        SampleFormat::I16 => build::<i16>(&device, stream_config, consumer, |s| (s * 32767.0) as i16),
        other => return Err(format!("unsupported output format {other}")),
    }?;
    voices.add(first);
    stream.play().map_err(|e| e.to_string())?;
    while let Ok(sound) = rx.recv_timeout((voices.ends + TAIL).saturating_duration_since(Instant::now())) {
        voices.add(sound);
    }
    Ok(())
}

struct Voices {
    producer: rtrb::Producer<Arc<[f32]>>,
    rate: u32,
    /// So the audio thread never frees a voice: it only lets go of its reference.
    held: Vec<Arc<[f32]>>,
    ends: Instant,
    last: Option<(Sound, Instant)>,
}

impl Voices {
    fn add(&mut self, (sounds, sound): (Sounds, Sound)) {
        // Two reports of one failure would play twice as loud.
        if self.last.is_some_and(|(last, at)| last == sound && at.elapsed() < Duration::from_millis(200)) {
            return;
        }
        let samples: Arc<[f32]> = chime(sounds, sound, self.rate).into();
        let length = Duration::from_secs_f32(samples.len() as f32 / self.rate as f32);
        if self.producer.push(samples.clone()).is_ok() {
            let now = Instant::now();
            self.ends = self.ends.max(now + length);
            self.held.push(samples);
            self.last = Some((sound, now));
        }
    }
}

fn build<T: SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: StreamConfig,
    mut incoming: rtrb::Consumer<Arc<[f32]>>,
    convert: fn(f32) -> T,
) -> Result<cpal::Stream, String> {
    let channels = config.channels.max(1) as usize;
    let mut playing: Vec<(Arc<[f32]>, usize)> = Vec::with_capacity(VOICES);
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| {
                while playing.len() < VOICES
                    && let Ok(voice) = incoming.pop()
                {
                    playing.push((voice, 0));
                }
                for frame in out.chunks_mut(channels) {
                    let mut mix = 0.0;
                    for (samples, pos) in &mut playing {
                        if let Some(s) = samples.get(*pos) {
                            mix += *s;
                            *pos += 1;
                        }
                    }
                    frame.fill(convert(mix.clamp(-1.0, 1.0)));
                }
                playing.retain(|(samples, pos)| *pos < samples.len());
            },
            |err| debug!("sound: {err}"),
            Some(Duration::from_secs(1)),
        )
        .map_err(|e| e.to_string())
}

/// A mode of what's struck: a multiple of the strike's pitch, falling to 1/e in `decay` seconds.
struct Partial {
    ratio: f64,
    gain: f64,
    decay: f64,
}

/// Noise through a band-pass at a multiple of the strike's pitch: the contact itself.
struct Click {
    gain: f64,
    decay: f64,
    ratio: f64,
    q: f64,
}

struct Timbre {
    partials: &'static [Partial],
    clicks: &'static [Click],
    attack: f64,
    /// The pitch starts at this multiple and settles to 1 with `glide_time`, so a strike pops.
    glide: f64,
    glide_time: f64,
}

/// A bubble's pop: the pitch drops into place within a few milliseconds.
const POP: Timbre = Timbre {
    partials: &[Partial { ratio: 1.0, gain: 1.0, decay: 0.04 }, Partial { ratio: 2.0, gain: 0.12, decay: 0.015 }],
    clicks: &[Click { gain: 0.25, decay: 0.0015, ratio: 3.0, q: 1.0 }],
    attack: 0.001,
    glide: 1.7,
    glide_time: 0.006,
};

/// A struck glass: soft onset, ringing out with two fading overtones.
const GLASS: Timbre = Timbre {
    partials: &[
        Partial { ratio: 1.0, gain: 1.0, decay: 0.11 },
        Partial { ratio: 2.0, gain: 0.25, decay: 0.055 },
        Partial { ratio: 3.0, gain: 0.08, decay: 0.0363 },
    ],
    clicks: &[],
    attack: 0.004,
    glide: 1.0,
    glide_time: 0.0,
};

/// Pops and glass rise a fifth for start and fall it for stop; an error repeats the lower note,
/// far enough apart to be heard as two.
fn chime(sounds: Sounds, sound: Sound, rate: u32) -> Vec<f32> {
    const E5: f64 = 659.26;
    const B5: f64 = 987.77;
    const C6: f64 = 1046.5;
    const G6: f64 = 1567.98;
    let rate = rate as f64;
    match (sounds, sound) {
        (Sounds::Off, _) => Vec::new(),
        (Sounds::Snap, Sound::Start) => render(&POP, &[(0.0, C6), (0.045, G6)], rate),
        (Sounds::Snap, Sound::Stop) => render(&POP, &[(0.0, G6), (0.045, C6)], rate),
        (Sounds::Snap, Sound::Error) => render(&POP, &[(0.0, C6), (0.09, C6)], rate),
        (Sounds::Glass, Sound::Start) => render(&GLASS, &[(0.0, E5), (0.07, B5)], rate),
        (Sounds::Glass, Sound::Stop) => render(&GLASS, &[(0.0, B5), (0.07, E5)], rate),
        (Sounds::Glass, Sound::Error) => render(&GLASS, &[(0.0, E5), (0.12, E5)], rate),
    }
}

/// `strikes` are (onset s, Hz).
fn render(timbre: &Timbre, strikes: &[(f64, f64)], rate: f64) -> Vec<f32> {
    let ring = timbre.partials.iter().map(|p| p.decay).chain(timbre.clicks.iter().map(|c| c.decay)).fold(0.0, f64::max);
    let last = strikes.iter().map(|&(at, _)| at).fold(0.0, f64::max);
    let mut out = vec![0.0f64; ((last + 7.0 * ring + 0.01) * rate) as usize];
    let mut seed: u32 = 0x9e37_79b9;
    for &(at, pitch) in strikes {
        let mut phases = vec![0.0f64; timbre.partials.len()];
        let mut filters: Vec<BandPass> =
            timbre.clicks.iter().map(|c| BandPass::new(pitch * c.ratio, c.q, rate)).collect();
        for (i, sample) in out[(at * rate) as usize..].iter_mut().enumerate() {
            let t = i as f64 / rate;
            let glide =
                if timbre.glide_time > 0.0 { 1.0 + (timbre.glide - 1.0) * (-t / timbre.glide_time).exp() } else { 1.0 };
            let hz = pitch * glide;
            let mut v = 0.0;
            for (phase, p) in phases.iter_mut().zip(timbre.partials) {
                if hz * p.ratio < rate * 0.45 {
                    *phase = (*phase + TAU * hz * p.ratio / rate) % TAU;
                    v += p.gain * phase.sin() * (-t / p.decay).exp();
                }
            }
            v *= (t / timbre.attack).min(1.0);
            if !timbre.clicks.is_empty() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let noise = seed as f64 / u32::MAX as f64 * 2.0 - 1.0;
                for (c, filter) in timbre.clicks.iter().zip(&mut filters) {
                    v += c.gain * 4.0 * filter.run(noise * (-t / c.decay).exp());
                }
            }
            *sample += v;
        }
    }
    level(out, rate)
}

/// As loud as the loudest 50 ms, so short and ringing sounds match, never past −6 dBFS, and
/// fading out at the end.
fn level(out: Vec<f64>, rate: f64) -> Vec<f32> {
    let window = (0.05 * rate) as usize;
    let loudest = (0..out.len())
        .step_by(window / 2)
        .map(|i| {
            let w = &out[i..(i + window).min(out.len())];
            (w.iter().map(|x| x * x).sum::<f64>() / w.len() as f64).sqrt()
        })
        .fold(0.0, f64::max);
    let peak = out.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    let scale = (10f64.powf(-21.5 / 20.0) / loudest).min(0.5 / peak);
    let fade = 0.01 * rate;
    let n = out.len();
    out.iter().enumerate().map(|(i, x)| (x * scale * ((n - i) as f64 / fade).min(1.0)) as f32).collect()
}

/// RBJ band-pass with a 0 dB peak.
struct BandPass {
    b0: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BandPass {
    fn new(hz: f64, q: f64, rate: f64) -> Self {
        let w = TAU * (hz / rate).min(0.45);
        let alpha = w.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self { b0: alpha / a0, a1: -2.0 * w.cos() / a0, a2: (1.0 - alpha) / a0, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    fn run(&mut self, x: f64) -> f64 {
        // b1 is 0 and b2 is −b0.
        let y = self.b0 * (x - self.x2) - self.a1 * self.y1 - self.a2 * self.y2;
        (self.x2, self.x1, self.y2, self.y1) = (self.x1, x, self.y1, y);
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_set_is_short_soft_and_fades_out() {
        for sounds in [Sounds::Snap, Sounds::Glass] {
            for sound in [Sound::Start, Sound::Stop, Sound::Error] {
                for rate in [44_100, 48_000] {
                    let samples = chime(sounds, sound, rate);
                    let seconds = samples.len() as f32 / rate as f32;
                    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    assert!(seconds < 1.0, "{sounds:?} {sound:?} lasts {seconds} s");
                    assert!((0.1..=0.5).contains(&peak), "{sounds:?} {sound:?} peaks at {peak}");
                    let tail = &samples[samples.len() - 5..];
                    assert!(tail.iter().all(|s| s.abs() < 0.01), "{sounds:?} {sound:?} ends in a click: {tail:?}");
                }
            }
            assert_ne!(chime(sounds, Sound::Start, 48_000), chime(sounds, Sound::Stop, 48_000), "{sounds:?}");
        }
        assert!(chime(Sounds::Off, Sound::Start, 48_000).is_empty());
    }
}
