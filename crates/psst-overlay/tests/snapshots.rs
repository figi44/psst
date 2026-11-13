use std::{
    fs,
    path::{Path, PathBuf},
};

use psst_overlay::{Levels, OverlayState, Pixmap, Renderer, Style};
use tiny_skia::{Color, PixmapPaint, Transform};

const WIDTH: u32 = 240;
const HEIGHT: u32 = 44;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("overlay-snapshots");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn speech() -> Levels {
    let mut seed: u32 = 0x5eed;
    (0..64)
        .map(|i| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let noise = (seed >> 16) as f32 / 65_536.0;
            let envelope = match i % 16 {
                0..=1 => 0.01,
                2..=6 => 0.12,
                7 => 0.02,
                _ => 0.25,
            };
            envelope * (0.5 + noise)
        })
        .collect()
}

fn cases() -> Vec<(&'static str, OverlayState, Levels)> {
    let none = Levels::new;
    vec![
        ("starting", OverlayState::Starting, none()),
        ("recording", OverlayState::Recording { level: 0.2 }, speech()),
        ("recording-just-started", OverlayState::Recording { level: 0.05 }, [0.05, 0.1, 0.08].into_iter().collect()),
        ("offline", OverlayState::Offline { level: 0.2, reason: "server unreachable".into() }, speech()),
        ("loading-model", OverlayState::LoadingModel, none()),
        ("processing", OverlayState::Processing, none()),
        ("sending", OverlayState::Sending, none()),
        ("retrying", OverlayState::Retrying, none()),
        ("waiting-for-server", OverlayState::WaitingForServer, none()),
        ("nothing-heard", OverlayState::NothingHeard, none()),
        ("done", OverlayState::Done, none()),
        ("error", OverlayState::Error { message: "Saved \u{b7} psst retry".into() }, none()),
        (
            "error-long",
            OverlayState::Error { message: "The server rejected the session: model unavailable (downloading)".into() },
            none(),
        ),
        ("flash", OverlayState::Flash { message: "Recording had a gap".into() }, none()),
        ("limit-warning", OverlayState::LimitWarning { seconds_left: 28 }, speech()),
    ]
}

const FRACTIONAL: [&str; 3] = ["recording", "processing", "error-long"];

fn scales(name: &str) -> Vec<f64> {
    if FRACTIONAL.contains(&name) { vec![1.0, 1.25, 2.0] } else { vec![1.0, 2.0] }
}

fn file_name(name: &str, scale: f64) -> String {
    format!("{name}@{scale}x.png")
}

/// Through PNG, so the comparison sees exactly what a fixture holds.
fn normalised(p: &Pixmap) -> Pixmap {
    Pixmap::decode_png(&p.encode_png().unwrap()).unwrap()
}

fn max_diff(a: &Pixmap, b: &Pixmap) -> Option<u8> {
    if (a.width(), a.height()) != (b.width(), b.height()) {
        return None;
    }
    Some(a.data().iter().zip(b.data()).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0))
}

#[test]
fn every_state_matches_its_snapshot() {
    let update = std::env::var_os("PSST_UPDATE_SNAPSHOTS").is_some();
    let mut renderer = Renderer::new();
    let style = Style::default();
    let mut failures = Vec::new();
    let mut expected_files = Vec::new();
    let mut sheet = Vec::new();

    for (name, state, levels) in cases() {
        for scale in scales(name) {
            let file = file_name(name, scale);
            expected_files.push(file.clone());
            let rendered = normalised(&renderer.render(&state, &levels, &style, WIDTH, HEIGHT, scale));
            if scale == 2.0 {
                sheet.push(rendered.clone());
            }
            let path = fixtures().join(&file);
            if update {
                fs::create_dir_all(fixtures()).unwrap();
                fs::write(&path, rendered.encode_png().unwrap()).unwrap();
                continue;
            }
            let fixture = Pixmap::load_png(&path);
            let diff = fixture.as_ref().ok().and_then(|f| max_diff(f, &rendered));
            // Allow rounding noise; anything visible fails.
            if diff.is_none_or(|d| d > 2) {
                let actual = out_dir().join(&file);
                fs::write(&actual, rendered.encode_png().unwrap()).unwrap();
                failures.push(format!("{file}: max channel diff {diff:?}; rendered: {}", actual.display()));
            }
        }
    }
    write_contact_sheet(&sheet);

    let mut stale: Vec<String> = fs::read_dir(fixtures())
        .map(|dir| {
            dir.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|f| f.ends_with(".png") && !expected_files.contains(f))
                .collect()
        })
        .unwrap_or_default();
    stale.sort();
    if update {
        for f in &stale {
            fs::remove_file(fixtures().join(f)).unwrap();
        }
        return;
    }
    assert!(stale.is_empty(), "fixtures without a case (regenerate to remove them): {stale:?}");
    assert!(
        failures.is_empty(),
        "{} snapshot(s) differ; if the change is intended, run with PSST_UPDATE_SNAPSHOTS=1:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn write_contact_sheet(frames: &[Pixmap]) {
    let Some(first) = frames.first() else { return };
    let (fw, fh) = (first.width(), first.height());
    let (pad, gap) = (32, 20);
    let columns = [Color::from_rgba8(0xe4, 0xe6, 0xeb, 0xff), Color::from_rgba8(0x22, 0x25, 0x2d, 0xff)];
    let col_w = fw + 2 * pad;
    let height = 2 * pad + frames.len() as u32 * (fh + gap) - gap;
    let mut sheet = Pixmap::new(col_w * columns.len() as u32, height).unwrap();
    for (c, bg) in columns.iter().enumerate() {
        let mut column = Pixmap::new(col_w, height).unwrap();
        column.fill(*bg);
        for (i, frame) in frames.iter().enumerate() {
            let y = pad + i as u32 * (fh + gap);
            column.draw_pixmap(
                pad as i32,
                y as i32,
                frame.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        sheet.draw_pixmap(
            (c as u32 * col_w) as i32,
            0,
            column.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    fs::write(out_dir().join("contact-sheet.png"), sheet.encode_png().unwrap()).unwrap();
}

#[test]
fn renders_any_size_and_scale_without_panicking() {
    let mut renderer = Renderer::new();
    let style = Style::default();
    let sizes = [(1, 1), (40, 44), (600, 80), (240, 20)];
    // Every state's icon at the degenerate sizes, and one state per layout (a label, a long label,
    // the waveform, and the waveform with text on either side) at every size too.
    let every_size = ["starting", "error-long", "recording", "offline", "limit-warning"];
    for (name, state, levels) in cases() {
        let sizes = if every_size.contains(&name) { &sizes[..] } else { &sizes[..2] };
        for &(w, h) in sizes {
            for scale in [0.5, 1.0, 1.75, 3.0, f64::NAN] {
                let p = renderer.render(&state, &levels, &style, w, h, scale);
                assert!(p.width() >= 1 && p.height() >= 1);
            }
        }
    }
}
