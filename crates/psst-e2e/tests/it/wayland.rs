use crate::support;

use std::{
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use psst_testkit::{
    Sandbox, headless_sway, nested_niri,
    wayland::{ClipboardOwner, HostKeyboard, TEXT_MIMES, keys, read_clipboard, screenshot},
};
use psst_wayland::{
    Chord, DataControl, Listing, MenuAction, MenuOutcome, MenuSpec, ModelChoice, Options, OverlayConfig, OverlayState,
    Rgba, Style, WaylandCmd, WaylandEvent, WaylandThread,
};
use serde_json::Value;
use support::{App, menu_bounds, now_us, pill_bounds};

const RESTORE: Duration = Duration::from_millis(1500);
/// For the tests that check nothing comes back: their probes wait for a restore that's due all the
/// same, so a shorter delay keeps the check and costs less.
const NO_RESTORE: Duration = Duration::from_millis(300);
const DICTATED: &str = "Dictated: Příliš žluťoučký kůň úpěl ďábelské ódy — ✓ 42";

/// Connected by path: this process's `WAYLAND_DISPLAY` may be the live session's.
struct Paster {
    thread: WaylandThread,
    rx: Receiver<WaylandEvent>,
}

impl Paster {
    fn new(sandbox: &Sandbox, restore_after: Duration) -> Self {
        Self::with(sandbox, Options { restore_after, ..Options::default() })
    }

    fn with(sandbox: &Sandbox, options: Options) -> Self {
        let options = Options { display: Some(sandbox.wayland_socket().to_path_buf()), ..options };
        let (tx, rx) = mpsc::channel();
        let thread = WaylandThread::spawn(options, move |event| {
            let _ = tx.send(event);
        })
        .expect("spawn the Wayland thread");
        Self { thread, rx }
    }

    fn send(&self, cmd: WaylandCmd) {
        self.thread.send(cmd).expect("the Wayland thread runs");
    }

    fn expect(&self, want: &WaylandEvent, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) if &event == want => return,
                Ok(_) => {}
                Err(_) => panic!("no {want:?} within {timeout:?}"),
            }
        }
    }

    /// The daemon snapshots at `toggle`, so the read has the recording's time; tests of the
    /// snapshot's own timing send it and paste right after.
    fn dictate(&self, generation: u64, text: &str, chord: Chord) -> Duration {
        self.send(WaylandCmd::SnapshotClipboard { generation });
        thread::sleep(Duration::from_millis(300));
        self.paste(generation, text, chord)
    }

    fn paste(&self, generation: u64, text: &str, chord: Chord) -> Duration {
        let ready = Instant::now();
        self.send(WaylandCmd::Paste { generation, text: text.into(), chord, delay_ms: 0 });
        self.expect(&WaylandEvent::Pasted { generation }, Duration::from_secs(3));
        ready.elapsed()
    }
}

fn text(v: &Value) -> Option<&str> {
    v["text"].as_str()
}

fn mimes(v: &Value) -> Vec<String> {
    v["mimes"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default()
}

#[track_caller]
fn assert_text(v: &Value, want: &str) {
    assert_eq!(text(v), Some(want), "the app got {v}");
}

#[track_caller]
fn assert_five_text_types(v: &Value) {
    let got = mimes(v);
    assert!(got.len() == 5 && TEXT_MIMES.iter().all(|t| got.iter().any(|g| g == t)), "offered {got:?}");
}

fn wait_for_restore(pasted_at: Instant, restore: Duration) {
    let due = pasted_at + restore + Duration::from_millis(500);
    thread::sleep(due.saturating_duration_since(Instant::now()));
}

/// Retries for 5 s: on a loaded machine the restore can land late, and a probe while it lands
/// reads nothing.
fn probe_restored(app: &mut App, pasted_at: Instant, restore: Duration, restored: impl Fn(&Value) -> bool) -> Value {
    thread::sleep((pasted_at + restore).saturating_duration_since(Instant::now()));
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        let probe = app.probe();
        if restored(&probe) || Instant::now() > give_up {
            return probe;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn probe_restored_text(app: &mut App, pasted_at: Instant, restore: Duration, want: &str) -> Value {
    probe_restored(app, pasted_at, restore, |probe| text(probe) == Some(want))
}

fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn fnv1a(data: &[u8]) -> String {
    let h = data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{h:016x}")
}

#[test]
fn the_sandbox_is_offscreen_and_isolated() {
    let sandbox = nested_niri();
    let socket = sandbox.wayland_socket();
    assert!(socket.starts_with(sandbox.root.path().canonicalize().unwrap()), "{}", socket.display());
    for var in ["XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "NIRI_SOCKET"] {
        let value = sandbox.var(var).unwrap_or_else(|| panic!("{var} is set"));
        assert!(std::path::Path::new(value).starts_with(sandbox.root.path().canonicalize().unwrap()), "{var}");
    }
    let out = sandbox.niri_msg(&["--json", "keyboard-layouts"]);
    let layouts: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(layouts["names"].as_array().map(Vec::len), Some(2), "{layouts}");
    let out = sandbox
        .command("sh")
        .args(["-c", "echo \"$WAYLAND_SOCKET|$DISPLAY|$DBUS_SESSION_BUS_ADDRESS\""])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "||disabled:");
}

#[test]
fn pastes_plain_text_and_restores_the_clipboard() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, RESTORE);
    let mut app = App::start(&sandbox);
    let _owner = ClipboardOwner::new(
        &sandbox,
        &[("text/plain;charset=utf-8", "orig plain ✓".as_bytes()), ("text/plain", "orig plain ✓".as_bytes())],
    );
    // The overlay is up while pasting, and takes no focus.
    paster.send(WaylandCmd::Overlay(Some(OverlayState::Processing)));
    paster.send(WaylandCmd::SnapshotClipboard { generation: 1 });
    thread::sleep(Duration::from_millis(300));
    let pasting_us = now_us();
    paster.paste(1, DICTATED, Chord::CtrlV);
    let pasted_at = Instant::now();
    let got = app.pasted();
    assert_text(&got, DICTATED);
    assert_eq!(got["chord"], "ctrl+v");
    paster.send(WaylandCmd::Overlay(None));

    let during = app.probe();
    assert_text(&during, DICTATED);
    assert_five_text_types(&during);

    // Not before the restore is due. The app's clock says when it had read the clipboard, counted
    // from before the paste, so a probe that comes late on a loaded machine checks nothing rather
    // than taking the restore for an early one...
    thread::sleep((RESTORE - Duration::from_millis(500)).saturating_sub(pasted_at.elapsed()));
    let probe = app.probe();
    let probed_after = Duration::from_micros(probe["t_done"].as_u64().unwrap() - pasting_us);
    if probed_after < RESTORE {
        assert_text(&probe, DICTATED);
    }
    // ...but after.
    let after = probe_restored_text(&mut app, pasted_at, RESTORE, "orig plain ✓");
    assert_text(&after, "orig plain ✓");
    assert_five_text_types(&after);
    let clipboard = read_clipboard(&sandbox).expect("a clipboard");
    assert!(clipboard.text_under_all_types(), "{clipboard:?}");
}

#[test]
fn an_image_comes_back_byte_for_byte() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, RESTORE);
    let mut app = App::start(&sandbox);
    let png = pseudo_random(300 * 1024, 7);
    let _owner = ClipboardOwner::new(&sandbox, &[("image/png", &png)]);
    paster.dictate(1, DICTATED, Chord::CtrlV);
    let pasted_at = Instant::now();
    assert_text(&app.pasted(), DICTATED);
    let after = probe_restored(&mut app, pasted_at, RESTORE, |probe| mimes(probe) == ["image/png"]);
    assert_eq!(mimes(&after), ["image/png"]);
    assert_eq!(after["blobs"]["image/png"]["len"], png.len());
    assert_eq!(after["blobs"]["image/png"]["fnv"], fnv1a(&png));
    assert_eq!(read_clipboard(&sandbox).unwrap().data["image/png"], png);
}

#[test]
fn a_copy_during_the_window_is_kept() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, RESTORE);
    let mut app = App::start(&sandbox);
    let _owner = ClipboardOwner::new(&sandbox, &[("text/plain;charset=utf-8", b"orig uc")]);
    paster.dictate(1, "dictation uc", Chord::CtrlV);
    let pasted_at = Instant::now();
    assert_text(&app.pasted(), "dictation uc");
    thread::sleep(Duration::from_millis(500));
    let _user = ClipboardOwner::new(&sandbox, &[("text/plain;charset=utf-8", b"user copied this")]);
    wait_for_restore(pasted_at, RESTORE);
    assert_text(&app.probe(), "user copied this");
}

#[test]
fn unreadable_snapshots_paste_without_restoring() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, NO_RESTORE);
    let mut app = App::start(&sandbox);
    // Over 10 MB: refused.
    let big = pseudo_random(11 * 1024 * 1024, 11);
    let owner = ClipboardOwner::new(&sandbox, &[("application/octet-stream", &big)]);
    paster.send(WaylandCmd::SnapshotClipboard { generation: 1 });
    thread::sleep(Duration::from_millis(300));
    paster.paste(1, "dictation big", Chord::CtrlV);
    let pasted_at = Instant::now();
    assert_text(&app.pasted(), "dictation big");
    wait_for_restore(pasted_at, NO_RESTORE);
    assert_text(&app.probe(), "dictation big");
    drop(owner);
    // Answers after 300 ms, before the restore is due: the paste waits at most 100 ms for it, then
    // goes without a restore.
    let _slow =
        ClipboardOwner::slow(&sandbox, &[("text/plain;charset=utf-8", b"orig slow")], Duration::from_millis(300));
    paster.send(WaylandCmd::SnapshotClipboard { generation: 2 });
    let took = paster.paste(2, "dictation slow", Chord::CtrlV);
    let pasted_at = Instant::now();
    assert!(took >= Duration::from_millis(90) && took < Duration::from_millis(400), "the paste took {took:?}");
    assert_text(&app.pasted(), "dictation slow");
    wait_for_restore(pasted_at, NO_RESTORE);
    assert_text(&app.probe(), "dictation slow");
}

#[test]
fn a_silent_clipboard_times_out_and_the_paste_goes_ahead() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, NO_RESTORE);
    let mut app = App::start(&sandbox);
    let _silent =
        ClipboardOwner::slow(&sandbox, &[("text/plain;charset=utf-8", b"orig never")], Duration::from_secs(4));
    paster.send(WaylandCmd::SnapshotClipboard { generation: 1 });
    // The read gives up after 2 s; a paste after that doesn't wait.
    thread::sleep(Duration::from_millis(2300));
    let took = paster.paste(1, "dictation timeout", Chord::CtrlV);
    let pasted_at = Instant::now();
    assert!(took < Duration::from_millis(80), "the paste took {took:?}");
    assert_text(&app.pasted(), "dictation timeout");
    wait_for_restore(pasted_at, NO_RESTORE);
    assert_text(&app.probe(), "dictation timeout");
}

#[test]
fn copy_only_sets_the_clipboard_without_pasting_or_restoring() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, NO_RESTORE);
    let mut app = App::start(&sandbox);
    let _owner = ClipboardOwner::new(&sandbox, &[("text/plain;charset=utf-8", b"orig copy")]);
    paster.send(WaylandCmd::SnapshotClipboard { generation: 1 });
    paster.send(WaylandCmd::Copy { generation: 1, text: "copied, not pasted".into() });
    paster.expect(&WaylandEvent::Copied { generation: 1 }, Duration::from_secs(3));
    let copied_at = Instant::now();
    thread::sleep(Duration::from_millis(300));
    assert!(app.pending().iter().all(|v| v["ev"] != "paste" && v["ev"] != "key"));
    let now = app.probe();
    assert_text(&now, "copied, not pasted");
    assert_five_text_types(&now);
    wait_for_restore(copied_at, NO_RESTORE);
    assert_text(&app.probe(), "copied, not pasted");
}

#[test]
fn the_wlr_data_control_fallback_pastes_and_restores() {
    let sandbox = nested_niri();
    let paster = Paster::with(
        &sandbox,
        Options { restore_after: RESTORE, data_control: DataControl::Wlr, ..Options::default() },
    );
    let mut app = App::start(&sandbox);
    let _owner = ClipboardOwner::new(&sandbox, &[("text/plain;charset=utf-8", b"orig wlr")]);
    paster.dictate(1, DICTATED, Chord::CtrlV);
    let pasted_at = Instant::now();
    assert_text(&app.pasted(), DICTATED);
    assert_text(&probe_restored_text(&mut app, pasted_at, RESTORE, "orig wlr"), "orig wlr");
}

#[test]
fn us_and_cz_layouts_survive_the_paste() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, RESTORE);
    let app = App::start(&sandbox);
    let mut keyboard = HostKeyboard::new(&sandbox);
    let mut generation = 0;
    for (index, name, two) in [(0, "us", "2"), (1, "cz", "ě")] {
        sandbox.niri_msg(&["action", "switch-layout", &index.to_string()]);
        thread::sleep(Duration::from_millis(100));
        app.drain();
        keyboard.tap(keys::KEY_2);
        let key = app.wait("key", Duration::from_secs(3));
        assert_eq!(key["utf8"], two, "{name}: KEY_2 before the paste: {key}");

        generation += 1;
        app.drain();
        paster.dictate(generation, &format!("layout {name} ✓"), Chord::CtrlV);
        let events = app.until("paste", Duration::from_secs(3));
        assert_text(events.last().unwrap(), &format!("layout {name} ✓"));
        assert!(events.iter().any(|e| e["ev"] == "keymap" && e["name"] == "psst"), "{name}: psst's keymap: {events:?}");

        // The next physical key gets niri's keymap and layout back.
        thread::sleep(Duration::from_millis(100));
        app.drain();
        keyboard.tap(keys::KEY_2);
        let events = app.until("key", Duration::from_secs(3));
        assert_eq!(events.last().unwrap()["utf8"], two, "{name}: KEY_2 after the paste: {events:?}");
        assert_eq!(events.last().unwrap()["layout"], index, "{name}: the layout didn't change");

        // Super held while pasting: the app sees Ctrl only.
        app.drain();
        keyboard.press(keys::KEY_LEFTMETA);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.wait("mods", deadline.saturating_duration_since(Instant::now()))["logo"] != true {}
        generation += 1;
        paster.dictate(generation, &format!("super held {name}"), Chord::CtrlV);
        let got = app.pasted();
        keyboard.release(keys::KEY_LEFTMETA);
        assert_text(&got, &format!("super held {name}"));
        assert_eq!((got["mods"]["ctrl"].as_bool(), got["mods"]["logo"].as_bool()), (Some(true), Some(false)), "{got}");
    }
    sandbox.niri_msg(&["action", "switch-layout", "0"]);
}

#[test]
fn paste_timing() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, Duration::from_secs(60));
    let app = App::start(&sandbox);
    let runs = 60;
    let (mut dispatch, mut received) = (Vec::new(), Vec::new());
    for i in 0..runs {
        let text = format!("timing {i} ✓");
        app.drain();
        paster.send(WaylandCmd::SnapshotClipboard { generation: i });
        let ready = now_us();
        dispatch.push(paster.paste(i, &text, Chord::CtrlV).as_secs_f64() * 1000.0);
        let got = app.pasted();
        assert_text(&got, &text);
        received.push((got["t_done"].as_u64().unwrap() - ready) as f64 / 1000.0);
    }
    let summary = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        (at(0.5), at(0.95), v[v.len() - 1])
    };
    let (d50, d95, dmax) = summary(&mut dispatch);
    let (r50, r95, rmax) = summary(&mut received);
    eprintln!(
        "paste timing over {runs} pastes in nested niri (ms): text ready → Pasted p50 {d50:.2}, p95 {d95:.2}, max {dmax:.2}; text ready → app has the text p50 {r50:.2}, p95 {r95:.2}, max {rmax:.2}"
    );
    // Generous bounds, for loaded CI machines; the budget is 300–500 ms for the whole stop → paste.
    assert!(d95 < 50.0, "text ready → Pasted p95 {d95:.2} ms");
}

#[test]
fn the_overlay_shows_at_the_bottom_centre_and_goes_away() {
    overlay_at_the_bottom_centre(&nested_niri());
}

#[test]
fn the_overlay_shows_at_the_bottom_centre_on_sway() {
    overlay_at_the_bottom_centre(&headless_sway());
}

fn overlay_at_the_bottom_centre(sandbox: &Sandbox) {
    // The default black pill would vanish on sway's black desktop.
    let style = Style { background: Rgba::new(0x80, 0x80, 0x80, 0xff), ..Style::default() };
    let overlay = OverlayConfig { style, ..OverlayConfig::default() };
    let paster = Paster::with(sandbox, Options { restore_after: RESTORE, overlay, ..Options::default() });
    assert!(pill_bounds(&screenshot(sandbox)).is_none(), "the screen starts empty");
    // The first command also connects.
    let sent = Instant::now();
    paster.send(WaylandCmd::Overlay(Some(OverlayState::Starting)));
    paster.expect(&WaylandEvent::OverlayShown, Duration::from_secs(3));
    eprintln!("overlay on {:?}: command → OverlayShown (presented) {:?}", sandbox.compositor(), sent.elapsed());
    for level in [0.1, 0.4, 0.2, 0.6, 0.3] {
        paster.send(WaylandCmd::Overlay(Some(OverlayState::Recording { level })));
    }

    thread::sleep(Duration::from_millis(300));
    let shot = screenshot(sandbox);
    let (x0, y0, x1, y1) = pill_bounds(&shot).expect("the overlay is on screen");
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    // 240 × 44 at scale 1, centred, 130 px above the bottom edge.
    assert!((236..=242).contains(&w) && (40..=46).contains(&h), "pill {w}×{h} at ({x0}, {y0})");
    let centre = (x0 + x1) / 2;
    assert!(centre.abs_diff(shot.width / 2) <= 2, "centre {centre} on a {} px wide output", shot.width);
    let bottom_margin = shot.height - 1 - y1;
    assert!((126..=134).contains(&bottom_margin), "{bottom_margin} px above the bottom");
    let dot = shot.pixel(x0 + h / 2, y0 + h / 2);
    assert!(dot[0] > 200 && dot[1] < 120 && dot[2] < 120, "the recording dot is {dot:?}");

    // One update, then silence: the waveform keeps moving without any more.
    paster.send(WaylandCmd::Overlay(Some(OverlayState::Recording { level: 0.0 })));
    let bars = |shot: &psst_testkit::wayland::Screenshot| -> Vec<[u8; 4]> {
        (x0 + h..x1 - h / 2).flat_map(|x| (y0..=y1).map(move |y| (x, y))).map(|(x, y)| shot.pixel(x, y)).collect()
    };
    thread::sleep(Duration::from_millis(300));
    let before = bars(&screenshot(sandbox));
    thread::sleep(Duration::from_millis(250));
    assert_ne!(bars(&screenshot(sandbox)), before, "the waveform stopped in silence");

    paster.send(WaylandCmd::Overlay(None));
    thread::sleep(Duration::from_millis(300));
    assert!(pill_bounds(&screenshot(sandbox)).is_none(), "the overlay is gone");
    // Shown again: a new surface, a new OverlayShown.
    let sent = Instant::now();
    paster.send(WaylandCmd::Overlay(Some(OverlayState::Done)));
    paster.expect(&WaylandEvent::OverlayShown, Duration::from_secs(3));
    eprintln!("overlay on {:?}: shown again in {:?}", sandbox.compositor(), sent.elapsed());
}

#[test]
fn the_menu_takes_the_keyboard_and_hands_it_back() {
    let sandbox = nested_niri();
    let paster = Paster::new(&sandbox, RESTORE);
    let app = App::start(&sandbox);
    let mut keyboard = HostKeyboard::new(&sandbox);
    let spec = MenuSpec {
        profile: "en".into(),
        profiles: Listing::Ready(vec!["cs".into(), "en".into()]),
        model: Some(ModelChoice::Unload),
        saved: vec![],
        history: Listing::Loading,
    };

    paster.send(WaylandCmd::Menu(Some(spec.clone())));
    app.wait("leave", Duration::from_secs(3));
    let shot = screenshot(&sandbox);
    let (x0, _, x1, y1) = menu_bounds(&shot).expect("the menu is on screen");
    assert!((276..=284).contains(&(x1 - x0 + 1)), "the menu is {} px wide", x1 - x0 + 1);
    let up = shot.height - 1 - y1;
    assert!((126..=134).contains(&up), "where the pill goes: {up} px up");
    for key in [keys::KEY_DOWN, keys::KEY_DOWN, keys::KEY_ENTER, keys::KEY_A, keys::KEY_2, keys::KEY_ENTER] {
        keyboard.tap(key);
    }
    paster.expect(&WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::AddWord("a2".into()))), Duration::from_secs(3));
    let events = app.until("focus", Duration::from_secs(3));
    assert!(events.iter().all(|e| e["ev"] != "key"), "the menu's keys reached the app: {events:?}");
    keyboard.tap(keys::KEY_A);
    assert_eq!(app.wait("key", Duration::from_secs(3))["utf8"], "a");

    paster.send(WaylandCmd::Menu(Some(spec)));
    app.wait("leave", Duration::from_secs(3));
    keyboard.tap(keys::KEY_ESC);
    paster.expect(&WaylandEvent::Menu(MenuOutcome::Closed), Duration::from_secs(3));
    app.wait("focus", Duration::from_secs(3));
    thread::sleep(Duration::from_millis(100));
    assert!(menu_bounds(&screenshot(&sandbox)).is_none(), "the menu is gone");
}

#[test]
fn losing_the_compositor_fails_the_paste_and_reconnects_to_a_new_one() {
    // The thread connects through a link, so a second compositor can take the first one's place.
    let links = tempfile::Builder::new().prefix("psst-link.").tempdir_in("/tmp").unwrap();
    let link = links.path().join("wayland");
    let mut first = nested_niri();
    std::os::unix::fs::symlink(first.wayland_socket(), &link).unwrap();
    let (tx, rx) = mpsc::channel();
    let thread = WaylandThread::spawn(
        Options { display: Some(link.clone()), restore_after: RESTORE, ..Options::default() },
        move |event| {
            let _ = tx.send(event);
        },
    )
    .unwrap();
    let paster = Paster { thread, rx };
    let app = App::start(&first);
    paster.dictate(1, "before", Chord::CtrlV);
    assert_text(&app.pasted(), "before");
    drop(app);

    drop(std::mem::replace(&mut first, nested_niri()));
    paster.expect(&WaylandEvent::Disconnected, Duration::from_secs(5));
    std::fs::remove_file(&link).unwrap();
    paster.send(WaylandCmd::Paste { generation: 2, text: "nowhere".into(), chord: Chord::CtrlV, delay_ms: 0 });
    let failed = paster.rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(&failed, WaylandEvent::Failed { generation: 2, .. }), "{failed:?}");

    std::os::unix::fs::symlink(first.wayland_socket(), &link).unwrap();
    let app = App::start(&first);
    paster.dictate(3, "after", Chord::CtrlV);
    assert_text(&app.pasted(), "after");
}
