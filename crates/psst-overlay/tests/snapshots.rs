use std::{
    fs,
    path::{Path, PathBuf},
};

use psst_overlay::{
    HistoryEntry, Key, Listing, Menu, MenuSpec, ModelChoice, OverlayState, Pixmap, Renderer, Style, Wave,
};
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

const SPEAKING: Wave = Wave { level: 0.55, phase: 1.0 };

fn cases() -> Vec<(&'static str, OverlayState, Wave)> {
    let still = Wave::default();
    vec![
        ("starting", OverlayState::Starting, still),
        ("recording", OverlayState::Recording { level: 0.55 }, SPEAKING),
        ("recording-silence", OverlayState::Recording { level: 0.0 }, Wave { level: 0.0, phase: 2.5 }),
        ("offline", OverlayState::Offline { level: 0.55, reason: "server unreachable".into() }, SPEAKING),
        ("loading-model", OverlayState::LoadingModel, still),
        ("processing", OverlayState::Processing, still),
        ("sending", OverlayState::Sending, still),
        ("retrying", OverlayState::Retrying, still),
        ("waiting-for-server", OverlayState::WaitingForServer, still),
        ("nothing-heard", OverlayState::NothingHeard, still),
        ("done", OverlayState::Done, still),
        ("error", OverlayState::Error { message: "Saved \u{b7} psst retry".into() }, still),
        (
            "error-long",
            OverlayState::Error { message: "The server rejected the session: model unavailable (downloading)".into() },
            still,
        ),
        ("flash", OverlayState::Flash { message: "Recording had a gap".into() }, still),
        ("limit-warning", OverlayState::LimitWarning { level: 0.55, seconds_left: 28 }, SPEAKING),
    ]
}

fn menu(model: Option<ModelChoice>, profiles: Listing, keys: &[Key]) -> Menu {
    let spec = MenuSpec { profile: "en".into(), profiles, model, saved: vec![], history: Listing::Loading };
    keyed(spec, keys)
}

fn keyed(spec: MenuSpec, keys: &[Key]) -> Menu {
    let mut menu = Menu::new(spec);
    for key in keys {
        menu.key(key.clone());
    }
    menu
}

fn entry(key: &str, when: &str, text: Option<&str>) -> HistoryEntry {
    HistoryEntry { key: key.into(), when: when.into(), text: text.map(String::from) }
}

fn history(keys: &[Key]) -> Menu {
    let saved = vec![
        entry("a", "14:02", Some("Hi Jan, I looked at the logs and the deploy failed because of the cache")),
        entry("b", "13:30 \u{b7} 0:34", None),
    ];
    let past =
        vec![entry("c", "15:10", Some("Sounds good, see you at ten.")), entry("d", "Mon 9:12", Some("Ship it."))];
    let profiles = Listing::Ready(["en"].map(String::from).to_vec());
    let spec = MenuSpec {
        profile: "en".into(),
        profiles,
        model: Some(ModelChoice::Unload),
        saved,
        history: Listing::Ready(past),
    };
    keyed(spec, keys)
}

fn menu_cases() -> Vec<(&'static str, Menu)> {
    let ready = Listing::Ready(["cs", "en", "work"].map(String::from).to_vec());
    let typed = |text: &str| [Key::Text("3".into()), Key::Text(text.into())];
    vec![
        ("menu", menu(Some(ModelChoice::Unload), ready.clone(), &[])),
        ("menu-checking-model", menu(None, Listing::Loading, &[Key::Down])),
        ("menu-profiles", menu(Some(ModelChoice::Load), ready, &[Key::Text("2".into()), Key::Up])),
        ("menu-profiles-loading", menu(None, Listing::Loading, &[Key::Text("2".into())])),
        (
            "menu-profiles-failed",
            menu(None, Listing::Failed("cannot reach the server".into()), &[Key::Text("2".into())]),
        ),
        ("menu-add-word", menu(None, Listing::Loading, &[Key::Text("3".into())])),
        ("menu-add-word-typed", menu(None, Listing::Loading, &typed("Kubernetes"))),
        ("menu-add-word-long", menu(None, Listing::Loading, &typed("Pneumonoultramicroscopicsilicovolcanoconiosis"))),
        ("menu-with-saved", history(&[])),
        ("menu-history", history(&[Key::Enter])),
        ("menu-history-discarding", history(&[Key::Enter, Key::Down, Key::Delete])),
    ]
}

const FRACTIONAL: [&str; 5] = ["recording", "processing", "error-long", "menu-add-word-long", "menu-history"];

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

    let pills = cases().into_iter().map(|(name, state, wave)| (name, Some((state, wave)), None));
    let menus = menu_cases().into_iter().map(|(name, menu)| (name, None, Some(menu)));
    for (name, pill, menu) in pills.chain(menus) {
        for scale in scales(name) {
            let file = file_name(name, scale);
            expected_files.push(file.clone());
            let rendered = match (&pill, &menu) {
                (Some((state, wave)), _) => renderer.render(state, *wave, &style, WIDTH, HEIGHT, scale),
                (_, Some(menu)) => renderer.render_menu(menu, &style, scale),
                (None, None) => unreachable!(),
            };
            let rendered = normalised(&rendered);
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
    if frames.is_empty() {
        return;
    }
    let (pad, gap) = (32, 20);
    let columns = [Color::from_rgba8(0xe4, 0xe6, 0xeb, 0xff), Color::from_rgba8(0x22, 0x25, 0x2d, 0xff)];
    let col_w = frames.iter().map(Pixmap::width).max().unwrap() + 2 * pad;
    let height = 2 * pad + frames.iter().map(|f| f.height() + gap).sum::<u32>() - gap;
    let mut sheet = Pixmap::new(col_w * columns.len() as u32, height).unwrap();
    for (c, bg) in columns.iter().enumerate() {
        let mut column = Pixmap::new(col_w, height).unwrap();
        column.fill(*bg);
        let mut y = pad;
        for frame in frames {
            column.draw_pixmap(
                pad as i32,
                y as i32,
                frame.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            y += frame.height() + gap;
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
    for (name, state, wave) in cases() {
        let sizes = if every_size.contains(&name) { &sizes[..] } else { &sizes[..2] };
        for &(w, h) in sizes {
            for scale in [0.5, 1.0, 1.75, 3.0, f64::NAN] {
                let p = renderer.render(&state, wave, &style, w, h, scale);
                assert!(p.width() >= 1 && p.height() >= 1);
            }
        }
    }
    for (_, menu) in menu_cases() {
        for scale in [0.5, 1.75, 3.0, f64::NAN] {
            let p = renderer.render_menu(&menu, &style, scale);
            assert!(p.width() >= 1 && p.height() >= 1);
        }
    }
}
