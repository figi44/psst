//! Cycles the overlay through every state in an offscreen nested niri, through the real Wayland
//! thread, and saves what the screen shows for each state.
//!
//! ```sh
//! cargo run -p psst-overlay --example overlay-demo -- [--out DIR] [--hold-ms N]
//! ```
//!
//! Screenshots go to `DIR` (default `target/overlay-demo`), one PNG per state, cropped to the
//! bottom of the screen. The sandbox never opens a window on the desktop.

use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use psst_overlay::OverlayState;
use psst_testkit::{nested_niri, wayland::screenshot};
use psst_wayland::{Options, WaylandCmd, WaylandEvent, WaylandThread};
use tiny_skia::{IntSize, Pixmap};

fn main() {
    let mut out = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/overlay-demo"));
    let mut hold = Duration::from_millis(1200);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => out = value().into(),
            "--hold-ms" => hold = Duration::from_millis(value().parse().expect("milliseconds")),
            _ => panic!("usage: overlay-demo [--out DIR] [--hold-ms N]"),
        }
    }
    std::fs::create_dir_all(&out).expect("create the output directory");

    let sandbox = nested_niri();
    let (tx, rx) = mpsc::channel();
    let options = Options { display: Some(sandbox.wayland_socket().to_path_buf()), ..Options::default() };
    let wayland = WaylandThread::spawn(options, move |event| {
        let _ = tx.send(event);
    })
    .expect("spawn the Wayland thread");

    let states = [
        ("starting", OverlayState::Starting),
        ("recording", OverlayState::Recording { level: 0.0 }),
        ("offline", OverlayState::Offline { level: 0.0, reason: "server unreachable".into() }),
        ("limit-warning", OverlayState::LimitWarning { seconds_left: 28 }),
        ("loading-model", OverlayState::LoadingModel),
        ("processing", OverlayState::Processing),
        ("sending", OverlayState::Sending),
        ("retrying", OverlayState::Retrying),
        ("waiting-for-server", OverlayState::WaitingForServer),
        ("nothing-heard", OverlayState::NothingHeard),
        ("done", OverlayState::Done),
        ("error", OverlayState::Error { message: "Saved \u{b7} psst retry".into() }),
        ("flash", OverlayState::Flash { message: "Recording had a gap".into() }),
    ];
    let start = Instant::now();
    for (i, (name, state)) in states.into_iter().enumerate() {
        let shown_at = Instant::now();
        let mut first = true;
        while shown_at.elapsed() < hold {
            // The recording states get a level about 25 times a second, like the daemon sends.
            let t = start.elapsed().as_secs_f32();
            let level = (0.5 + 0.5 * (t * 7.0).sin() * (t * 1.3).cos()).powi(2) * 0.4;
            let state = match &state {
                OverlayState::Recording { .. } => OverlayState::Recording { level },
                OverlayState::Offline { reason, .. } => OverlayState::Offline { level, reason: reason.clone() },
                other => other.clone(),
            };
            if first || matches!(state, OverlayState::Recording { .. } | OverlayState::Offline { .. }) {
                wayland.send(WaylandCmd::Overlay(Some(state))).expect("the Wayland thread runs");
                first = false;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let shot = screenshot(&sandbox);
        let (w, h) = (shot.width.min(420), 140.min(shot.height));
        let (x0, y0) = ((shot.width - w) / 2, shot.height - h - 90);
        let mut crop = Vec::with_capacity((w * h * 4) as usize);
        for y in y0..y0 + h {
            let row = (y * shot.width + x0) as usize * 4;
            crop.extend_from_slice(&shot.rgba[row..row + w as usize * 4]);
        }
        let pixmap = Pixmap::from_vec(crop, IntSize::from_wh(w, h).expect("size")).expect("pixmap");
        let path = out.join(format!("{:02}-{name}.png", i + 1));
        pixmap.save_png(&path).expect("save the screenshot");
        println!("{name}: {}", path.display());
    }
    wayland.send(WaylandCmd::Overlay(None)).expect("the Wayland thread runs");
    drop(wayland);
    let shown = rx.try_iter().filter(|e| *e == WaylandEvent::OverlayShown).count();
    println!("OverlayShown: {shown}");
}
