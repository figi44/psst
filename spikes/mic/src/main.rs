//! Phase 0 mic spike: cpal 0.18 with its native PipeWire host.
//!
//!   mic-spike list        [--host pipewire|alsa]
//!   mic-spike first-frame [--host ..] [--runs N] [--device ID-SUBSTRING] [--buffer FRAMES]
//!   mic-spike capture     [--host ..] [--secs N] [--device ..]
//!
//! Every stream asks for 16 kHz mono f32. Audio is only measured (level,
//! frame counts, timestamps); nothing is saved.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, HostId, StreamConfig};

const RATE: u32 = 16_000;

/// `--buffer N` asks for N frames per callback (PipeWire: node.latency).
static BUFFER: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let flag = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let host_id = match flag("--host").as_deref() {
        None | Some("pipewire") => HostId::PipeWire,
        Some("alsa") => HostId::Alsa,
        Some(h) => panic!("unknown host {h}"),
    };
    let t = Instant::now();
    let host = cpal::host_from_id(host_id).expect("host");
    let host_ms = ms(t.elapsed());
    let device = flag("--device");
    BUFFER
        .set(flag("--buffer").map(|s| s.parse().expect("frames")))
        .unwrap();
    match cmd {
        "list" => list(&host, host_ms),
        "first-frame" => first_frame(
            &host,
            device,
            flag("--runs").map_or(30, |s| s.parse().unwrap()),
        ),
        "capture" => capture(
            &host,
            device,
            flag("--secs").map_or(30, |s| s.parse().unwrap()),
        ),
        _ => eprintln!("usage: see src/main.rs"),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn config() -> StreamConfig {
    let buffer_size = match BUFFER.get().copied().flatten() {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };
    StreamConfig {
        channels: 1,
        sample_rate: RATE,
        buffer_size,
    }
}

fn pick(host: &cpal::Host, want: &Option<String>) -> cpal::Device {
    match want {
        None => host
            .default_input_device()
            .expect("no default input device"),
        Some(w) => host
            .input_devices()
            .expect("input devices")
            .find(|d| {
                d.id()
                    .map(|id| id.to_string().contains(w.as_str()))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no input device matching {w:?}")),
    }
}

fn describe(d: &cpal::Device) -> String {
    let id = d
        .id()
        .map(|i| i.to_string())
        .unwrap_or_else(|e| format!("<{e}>"));
    let name = d
        .description()
        .map(|x| x.name().to_string())
        .unwrap_or_default();
    format!("{id} — {name}")
}

fn list(host: &cpal::Host, host_ms: f64) {
    println!("available hosts: {:?}", cpal::available_hosts());
    println!("host {:?} created in {host_ms:.1} ms", host.id());
    let t = Instant::now();
    match host.default_input_device() {
        Some(d) => {
            println!(
                "default input ({:.1} ms): {}",
                ms(t.elapsed()),
                describe(&d)
            );
            if let Ok(c) = d.default_input_config() {
                println!(
                    "  default config: {} ch, {} Hz, {:?}",
                    c.channels(),
                    c.sample_rate(),
                    c.sample_format()
                );
            }
        }
        None => println!("no default input device"),
    }
    let t = Instant::now();
    let devs: Vec<_> = host.input_devices().expect("input devices").collect();
    println!("{} input devices ({:.1} ms):", devs.len(), ms(t.elapsed()));
    for d in &devs {
        println!("  {}", describe(d));
        if let Ok(cfgs) = d.supported_input_configs() {
            for c in cfgs.take(4) {
                println!(
                    "      {} ch, {}–{} Hz, {:?}, buffer {:?}",
                    c.channels(),
                    c.min_sample_rate(),
                    c.max_sample_rate(),
                    c.sample_format(),
                    c.buffer_size()
                );
            }
        }
    }
}

fn stats(name: &str, v: &mut [f64]) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    println!(
        "{name:<26} {:>8.2} {:>8.2} {:>8.2} {:>8.2}",
        q(0.5),
        q(0.95),
        v[0],
        v[v.len() - 1]
    );
}

/// open (device lookup + build + play) → first callback with samples.
fn first_frame(host: &cpal::Host, want: Option<String>, runs: usize) {
    let (mut lookup, mut build, mut play, mut first, mut total) =
        (vec![], vec![], vec![], vec![], vec![]);
    let mut first_len = vec![];
    for _ in 0..runs {
        let (tx, rx) = mpsc::sync_channel::<(Instant, usize)>(1);
        let mut sent = false;
        let t0 = Instant::now();
        let dev = pick(host, &want);
        let t1 = Instant::now();
        let stream = dev
            .build_input_stream::<f32, _, _>(
                config(),
                move |data: &[f32], _| {
                    if !sent && !data.is_empty() {
                        sent = true;
                        let _ = tx.try_send((Instant::now(), data.len()));
                    }
                },
                |e| eprintln!("stream error: {e}"),
                None,
            )
            .expect("build_input_stream (16 kHz mono f32)");
        let t2 = Instant::now();
        stream.play().expect("play");
        let t3 = Instant::now();
        let (t4, n) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no audio within 5 s");
        drop(stream);
        lookup.push(ms(t1 - t0));
        build.push(ms(t2 - t1));
        play.push(ms(t3 - t2));
        first.push(ms(t4 - t3));
        total.push(ms(t4 - t0));
        first_len.push(n as f64);
        std::thread::sleep(Duration::from_millis(300));
    }
    println!(
        "# first-frame: host={:?}, {runs} runs, 16 kHz mono f32, buffer {:?}, device {}",
        host.id(),
        config().buffer_size,
        describe(&pick(host, &want))
    );
    println!(
        "{:<26} {:>8} {:>8} {:>8} {:>8}",
        "ms", "median", "p95", "min", "max"
    );
    stats("device lookup", &mut lookup);
    stats("build_input_stream", &mut build);
    stats("play()", &mut play);
    stats("play → first samples", &mut first);
    stats("open → first samples", &mut total);
    stats("first buffer (frames)", &mut first_len);
}

#[derive(Clone, Copy, Default)]
struct Rec {
    frames: u32,
    capture_ns: u128,
    wall: Option<Instant>,
}

/// Steady capture: gaps from capture timestamps, xruns from the error
/// callback, ring-buffer overflows with a 2 s SPSC ring drained every 20 ms
/// (as psst's writer would).
fn capture(host: &cpal::Host, want: Option<String>, secs: u64) {
    let dev = pick(host, &want);
    let ring_cap = (RATE * 2) as usize;
    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_cap);
    let recs: Arc<Mutex<Vec<Rec>>> = Arc::new(Mutex::new(Vec::with_capacity(200_000)));
    let overflow = Arc::new(AtomicU64::new(0));
    let errors: Arc<Mutex<Vec<(f64, String)>>> = Arc::default();
    let sumsq = Arc::new(Mutex::new(0f64));
    let start = Instant::now();
    let (r2, o2, s2, e2) = (
        recs.clone(),
        overflow.clone(),
        sumsq.clone(),
        errors.clone(),
    );
    let mut first: Option<cpal::StreamInstant> = None;
    let stream = dev
        .build_input_stream::<f32, _, _>(
            config(),
            move |data: &[f32], info| {
                let cap = info.timestamp().capture;
                let base = *first.get_or_insert(cap);
                let rec = Rec {
                    frames: data.len() as u32,
                    capture_ns: cap.duration_since(base).as_nanos(),
                    wall: Some(Instant::now()),
                };
                let pushed = match prod.write_chunk_uninit(data.len()) {
                    Ok(chunk) => chunk.fill_from_iter(data.iter().copied()),
                    Err(_) => 0,
                };
                if pushed < data.len() {
                    o2.fetch_add((data.len() - pushed) as u64, Ordering::Relaxed);
                }
                if let Ok(mut v) = r2.try_lock() {
                    v.push(rec);
                }
                let s: f64 = data.iter().map(|x| (*x as f64) * (*x as f64)).sum();
                if let Ok(mut t) = s2.try_lock() {
                    *t += s;
                }
            },
            move |e| {
                let kind = match e.kind() {
                    ErrorKind::Xrun => "xrun".to_string(),
                    k => format!("{k:?}: {e}"),
                };
                e2.lock().unwrap().push((ms(start.elapsed()), kind));
            },
            None,
        )
        .expect("build_input_stream (16 kHz mono f32)");
    stream.play().expect("play");
    // The writer: drains the ring every 20 ms; tracks its own lateness.
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut drained = 0u64;
    let mut worst_fill = 0usize;
    let mut worst_late = Duration::ZERO;
    let mut next = Instant::now();
    while Instant::now() < deadline {
        next += Duration::from_millis(20);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        worst_late = worst_late.max(Instant::now().saturating_duration_since(next));
        let n = cons.slots();
        worst_fill = worst_fill.max(n);
        if let Ok(chunk) = cons.read_chunk(n) {
            drained += chunk.len() as u64;
            chunk.commit_all();
        }
    }
    drop(stream);
    let elapsed = start.elapsed();
    let recs = recs.lock().unwrap().clone();
    let total: u64 = recs.iter().map(|r| r.frames as u64).sum();
    // Gaps: a buffer that starts later than the previous one's end.
    let mut gaps = 0;
    let mut gap_ms = 0.0;
    let mut max_cb_interval = Duration::ZERO;
    let mut sizes = std::collections::BTreeMap::<u32, u32>::new();
    for w in recs.windows(2) {
        let expected = w[0].capture_ns + w[0].frames as u128 * 1_000_000_000 / RATE as u128;
        let delta = w[1].capture_ns as i128 - expected as i128;
        // tolerance: half a buffer (cpal timestamps come from the graph clock)
        let tol = (w[0].frames as i128 * 1_000_000_000 / RATE as i128) / 2;
        if delta > tol {
            gaps += 1;
            gap_ms += delta as f64 / 1e6;
        }
        if let (Some(a), Some(b)) = (w[0].wall, w[1].wall) {
            max_cb_interval = max_cb_interval.max(b - a);
        }
    }
    for r in &recs {
        *sizes.entry(r.frames).or_default() += 1;
    }
    let rms = (*sumsq.lock().unwrap() / total.max(1) as f64).sqrt();
    let errors = errors.lock().unwrap().clone();
    println!(
        "# capture: host={:?}, {secs} s, device {}",
        host.id(),
        describe(&dev)
    );
    println!(
        "callbacks: {}, buffer sizes (frames: count): {sizes:?}",
        recs.len()
    );
    println!(
        "frames: {total} received vs {:.0} expected from wall clock ({:+.2}%)",
        elapsed.as_secs_f64() * RATE as f64,
        (total as f64 / (elapsed.as_secs_f64() * RATE as f64) - 1.0) * 100.0
    );
    println!("timestamp gaps (> half a buffer): {gaps} ({gap_ms:.1} ms total)");
    println!(
        "max wall interval between callbacks: {:.1} ms",
        ms(max_cb_interval)
    );
    println!(
        "stream errors: {} {:?}",
        errors.len(),
        errors.iter().take(10).collect::<Vec<_>>()
    );
    println!(
        "ring (2 s): overflowed samples {}, worst fill {:.1} ms, writer drained {drained}, writer worst lateness {:.1} ms",
        overflow.load(Ordering::Relaxed),
        worst_fill as f64 * 1000.0 / RATE as f64,
        ms(worst_late)
    );
    println!("level: RMS {rms:.5} (non-zero means real audio)");
}
