//! Driver for the paste prototype. Run only inside spikes/sandbox/sandbox.sh.
//!
//!   paste-spike checks [--proto ext|wlr] [--restore-ms N] [--only NAME,..]
//!   paste-spike bench  [--proto ext|wlr] [--runs N] [--delay-ms N] [--chord ctrl-v|ctrl-shift-v] [--keymap minimal] [--physical 1]
//!   paste-spike paste  --text T [--chord ..] [--delay-ms N]     one dictation, waits for restore
//!   paste-spike seed   [--text MIME=STR].. [--file MIME=PATH].. [--delay-ms N] [--proto ..]
//!   paste-spike clear | dump [--proto ..]
//!   paste-spike host-keys "press:125 sleep:100 tap:3 release:125"   (on the headless host)
//!
//! `checks` starts paste-target and verifies, through it, the text received
//! and the clipboard restore for each scenario; `bench` measures latency.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::OwnedFd;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_paste::clipboard::{Note, ReadError, TEXT_MIMES, choose_mime};
use wayland_paste::clock::now_us;
use wayland_paste::dc::{Bytes, DcDevice, DcHandler, DcManager, DcOffer, DcSource, Proto};
use wayland_paste::keymap;
use wayland_paste::paster::{self, Chord, Cmd, Evt, Options, PasteTiming};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

type Res<T> = Result<T, String>;

fn main() {
    wayland_paste::guard::assert_sandbox();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = args.split_first().map(|(c, r)| (c.as_str(), r.to_vec())).unwrap_or(("help", vec![]));
    let a = Args::parse(&rest);
    let r = match cmd {
        "checks" => checks(&a),
        "bench" => bench(&a),
        "paste" => one_paste(&a),
        "seed" => seed(&a),
        "clear" => clear(&a),
        "dump" => dump(&a),
        "host-keys" => host_keys(&a),
        _ => {
            eprintln!("usage: see the header of src/bin/paste-spike.rs");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- arguments

struct Args {
    flags: BTreeMap<String, Vec<String>>,
    positional: Vec<String>,
}

impl Args {
    fn parse(v: &[String]) -> Args {
        let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut positional = Vec::new();
        let mut it = v.iter();
        while let Some(s) = it.next() {
            if let Some(k) = s.strip_prefix("--") {
                let val = it.next().cloned().unwrap_or_default();
                flags.entry(k.to_string()).or_default().push(val);
            } else {
                positional.push(s.clone());
            }
        }
        Args { flags, positional }
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).and_then(|v| v.last()).map(|s| s.as_str())
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.flags.get(k).cloned().unwrap_or_default()
    }
    fn num(&self, k: &str, d: u64) -> u64 {
        self.get(k).map(|s| s.parse().expect("number")).unwrap_or(d)
    }
    fn proto(&self) -> Option<Proto> {
        self.get("proto").map(|s| s.parse().expect("proto"))
    }
    fn chord(&self) -> Chord {
        match self.get("chord").unwrap_or("ctrl-v") {
            "ctrl-v" => Chord::CtrlV,
            "ctrl-shift-v" => Chord::CtrlShiftV,
            c => panic!("unknown chord {c}"),
        }
    }
}

// ---------------------------------------------------------------- helpers

fn exe(name: &str) -> std::path::PathBuf {
    std::env::current_exe().unwrap().with_file_name(name)
}

struct Target {
    child: Child,
    rx: Receiver<Value>,
}

impl Target {
    fn spawn() -> Res<Target> {
        let mut child = Command::new(exe("paste-target"))
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn paste-target: {e}"))?;
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let t = Target { child, rx };
        t.wait("focus", Duration::from_secs(5))?;
        Ok(t)
    }

    /// Next event named `ev`; other events are dropped.
    fn wait(&self, ev: &str, timeout: Duration) -> Res<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) if v["ev"] == ev => return Ok(v),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("target: no {ev} event within {timeout:?}"));
                }
                Err(RecvTimeoutError::Disconnected) => return Err("target exited".into()),
            }
        }
    }

    /// All events until `ev` (inclusive).
    fn collect_until(&self, ev: &str, timeout: Duration) -> Res<Vec<Value>> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) => {
                    let done = v["ev"] == ev;
                    out.push(v);
                    if done {
                        return Ok(out);
                    }
                }
                Err(_) => return Err(format!("target: no {ev} event within {timeout:?}")),
            }
        }
    }

    fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Seeder(Child);

impl Drop for Seeder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `paste-spike seed` (a separate client owning the clipboard) and
/// waits until its selection is set.
fn spawn_seed(args: &[&str], proto: Proto) -> Res<Seeder> {
    let mut c = Command::new(exe("paste-spike"));
    c.arg("seed").args(args).arg("--proto").arg(proto_name(proto)).stdout(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("spawn seed: {e}"))?;
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).map_err(|e| e.to_string())?;
    if line.trim() != "ready" {
        return Err(format!("seed did not get ready: {line:?}"));
    }
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
    });
    Ok(Seeder(child))
}

fn proto_name(p: Proto) -> &'static str {
    match p {
        Proto::Ext => "ext",
        Proto::Wlr => "wlr",
    }
}

fn sandbox_file(name: &str, data: &[u8]) -> String {
    let root = std::env::var("XDG_STATE_HOME").expect("XDG_STATE_HOME");
    let p = format!("{root}/{name}");
    std::fs::write(&p, data).expect("write sandbox file");
    p
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
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn run_cmd(cmd: &str, args: &[&str]) -> Res<String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !o.status.success() {
        return Err(format!("{cmd} {args:?}: {}", String::from_utf8_lossy(&o.stderr)));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

// ---------------------------------------------------------------- checks

struct Ctx {
    p: paster::Handle,
    t: Target,
    proto: Proto,
    generation: u64,
    restore: Duration,
    notes: Vec<Note>,
}

impl Ctx {
    fn next_gen(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Waits for a paster event matching `f`, keeping notes.
    fn wait_evt<T>(&mut self, timeout: Duration, mut f: impl FnMut(&Evt) -> Option<T>) -> Res<T> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let e = self.p.rx.recv_timeout(left).map_err(|_| "paster: timed out".to_string())?;
            if let Evt::Note(n) = &e {
                self.notes.push(n.clone());
            }
            if let Some(v) = f(&e) {
                return Ok(v);
            }
        }
    }

    fn pump(&mut self) {
        while let Ok(e) = self.p.rx.try_recv() {
            if let Evt::Note(n) = e {
                self.notes.push(n);
            }
        }
    }

    fn snapshot(&mut self, generation: u64) {
        self.p.send(Cmd::Snapshot { generation });
    }

    /// Paste and wait for both `Pasted` and the target's paste event.
    fn paste(&mut self, generation: u64, text: &str, chord: Chord) -> Res<(PasteTiming, Value, Instant)> {
        self.t.drain();
        let ready = now_us();
        self.p.send(Cmd::Paste { generation, text: text.into(), chord, delay_ms: 0, t_ready_us: ready });
        let timing = self.wait_evt(Duration::from_secs(3), |e| match e {
            Evt::Pasted { generation: g, timing } if *g == generation => Some(timing.clone()),
            _ => None,
        })?;
        let at = Instant::now();
        let got = self.t.wait("paste", Duration::from_secs(3))?;
        Ok((timing, got, at))
    }

    /// Ctrl+V without touching the clipboard: what does an app see now?
    fn probe(&mut self) -> Res<Value> {
        self.t.drain();
        self.p.send(Cmd::ChordOnly { chord: Chord::CtrlV });
        self.t.wait("paste", Duration::from_secs(3))
    }

    fn sleep_until(&self, t: Instant) {
        let now = Instant::now();
        if t > now {
            std::thread::sleep(t - now);
        }
    }

    fn after_restore(&self, pasted_at: Instant) -> Instant {
        pasted_at + self.restore + Duration::from_millis(400)
    }

    fn noted(&self, f: impl Fn(&Note) -> bool) -> bool {
        self.notes.iter().any(f)
    }
}

fn text_of(v: &Value) -> Option<&str> {
    v["text"].as_str()
}

fn mimes_of(v: &Value) -> Vec<String> {
    v["mimes"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn expect_text(v: &Value, want: &str, what: &str) -> Res<()> {
    match text_of(v) {
        Some(t) if t == want => Ok(()),
        other => Err(format!("{what}: expected {want:?}, got {other:?} (mimes {:?})", mimes_of(v))),
    }
}

fn expect_text_mimes(v: &Value, what: &str) -> Res<()> {
    let m = mimes_of(v);
    if m.len() == TEXT_MIMES.len() && TEXT_MIMES.iter().all(|t| m.iter().any(|x| x == t)) {
        Ok(())
    } else {
        Err(format!("{what}: expected the five plain-text types, got {m:?}"))
    }
}

const DICTATED: &str = "Dictated: Příliš žluťoučký kůň úpěl ďábelské ódy — ✓ 42";

type Check = fn(&mut Ctx) -> Res<String>;

fn checks(a: &Args) -> Res<()> {
    let proto = a.proto();
    let restore = Duration::from_millis(a.num("restore-ms", 5000));
    let mut opts = Options { proto, ..Default::default() };
    opts.machine.restore_after = restore;
    // The paster first: on headless sway its virtual keyboard is the seat's
    // only keyboard, and the target needs one to get focus.
    let p = paster::spawn(opts)?;
    let t = Target::spawn()?;
    let proto = p.proto;
    let compositor = std::env::var("PSST_SANDBOX_COMPOSITOR").unwrap_or_default();
    println!("# checks: compositor={compositor} data-control={} restore_after={restore:?}", proto_name(proto));
    let mut ctx = Ctx { p, t, proto, generation: 0, restore, notes: vec![] };

    let mut all: Vec<(&str, Check)> = vec![
        ("plain-text", check_plain),
        ("rich-text", check_rich),
        ("image", check_image),
        ("two-quick-dictations", check_two_quick),
        ("manager-took-over", check_manager),
        ("user-copied-meanwhile", check_user_copy),
        ("external-change-before-paste", check_external_before_paste),
        ("empty-clipboard", check_empty),
        ("oversize-snapshot", check_oversize),
        ("slow-source", check_slow_source),
        ("snapshot-timeout", check_snapshot_timeout),
        ("ctrl-shift-v", check_ctrl_shift_v),
    ];
    if compositor == "niri" {
        all.push(("layouts-us-cz", check_layouts));
    }
    let only: Vec<String> = a.get("only").map(|s| s.split(',').map(String::from).collect()).unwrap_or_default();
    let mut failed = 0;
    for (name, f) in all {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        ctx.notes.clear();
        let start = Instant::now();
        let r = f(&mut ctx);
        ctx.pump();
        match r {
            Ok(detail) => println!("PASS {name} ({:.1}s): {detail}", start.elapsed().as_secs_f32()),
            Err(e) => {
                failed += 1;
                println!("FAIL {name}: {e}");
                for n in &ctx.notes {
                    println!("     note: {n:?}");
                }
            }
        }
    }
    println!("# {} failed", failed);
    if failed > 0 { Err(format!("{failed} checks failed")) } else { Ok(()) }
}

fn check_plain(c: &mut Ctx) -> Res<String> {
    let _s =
        spawn_seed(&["--text", "text/plain;charset=utf-8=orig plain ✓", "--text", "text/plain=orig plain ✓"], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    let (timing, got, at) = c.paste(g, DICTATED, Chord::CtrlV)?;
    expect_text(&got, DICTATED, "received")?;
    if got["chord"] != "ctrl+v" {
        return Err(format!("chord {}", got["chord"]));
    }
    let during = c.probe()?;
    expect_text(&during, DICTATED, "clipboard during the 5 s window")?;
    expect_text_mimes(&during, "dictation source")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "orig plain ✓", "restored")?;
    expect_text_mimes(&after, "restored source")?;
    Ok(format!(
        "received text matches; restored under 5 text types; dispatch {:.2} ms",
        (timing.sync2 - timing.ready) as f64 / 1000.0
    ))
}

fn check_rich(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(
        &[
            "--text",
            "text/html=<b>rich</b> <i>text</i>",
            "--text",
            "text/plain;charset=utf-8=rich text",
            "--text",
            "text/plain=rich text",
        ],
        c.proto,
    )?;
    let g = c.next_gen();
    c.snapshot(g);
    let (_, got, at) = c.paste(g, DICTATED, Chord::CtrlV)?;
    expect_text(&got, DICTATED, "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "rich text", "restored")?;
    expect_text_mimes(&after, "restored source")?;
    Ok("plain-text representation restored; text/html is not (one representation only)".into())
}

fn check_image(c: &mut Ctx) -> Res<String> {
    let png = pseudo_random(300 * 1024, 7);
    let path = sandbox_file("seed.png", &png);
    let _s = spawn_seed(&["--file", &format!("image/png={path}")], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    let (_, got, at) = c.paste(g, DICTATED, Chord::CtrlV)?;
    expect_text(&got, DICTATED, "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    if mimes_of(&after) != ["image/png"] {
        return Err(format!("restored mimes {:?}", mimes_of(&after)));
    }
    let b = &after["blobs"]["image/png"];
    if b["len"] != png.len() || b["fnv"] != fnv1a(&png) {
        return Err(format!("restored image differs: {b}"));
    }
    Ok(format!("300 KiB image/png restored byte-identical ({} bytes)", png.len()))
}

fn check_two_quick(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(&["--text", "text/plain;charset=utf-8=orig two"], c.proto)?;
    let g1 = c.next_gen();
    c.snapshot(g1);
    let (_, got1, at1) = c.paste(g1, "first dictation", Chord::CtrlV)?;
    expect_text(&got1, "first dictation", "first received")?;
    std::thread::sleep(Duration::from_millis(1000));
    let g2 = c.next_gen();
    c.snapshot(g2);
    let (_, got2, at2) = c.paste(g2, "second dictation", Chord::CtrlV)?;
    expect_text(&got2, "second dictation", "second received")?;
    c.sleep_until(c.after_restore(at1));
    let mid = c.probe()?;
    expect_text(&mid, "second dictation", "after the first timer (must be ignored)")?;
    c.sleep_until(c.after_restore(at2));
    let after = c.probe()?;
    expect_text(&after, "orig two", "restored after the second")?;
    c.pump();
    if !c.noted(|n| matches!(n, Note::SnapshotInherited { generation, from } if *generation == g2 && *from == g1)) {
        return Err("second snapshot did not inherit".into());
    }
    Ok("second snapshot inherited the first's; first timer ignored; original restored".into())
}

fn check_manager(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(&["--text", "text/plain;charset=utf-8=orig mgr"], c.proto)?;
    let g1 = c.next_gen();
    c.snapshot(g1);
    let (_, got1, _) = c.paste(g1, "first via manager", Chord::CtrlV)?;
    expect_text(&got1, "first via manager", "first received")?;
    // A clipboard manager takes ownership of the dictated text.
    let _m = spawn_seed(
        &["--text", "text/plain;charset=utf-8=first via manager", "--text", "text/plain=first via manager"],
        c.proto,
    )?;
    std::thread::sleep(Duration::from_millis(200));
    let g2 = c.next_gen();
    c.snapshot(g2);
    let (_, got2, at2) = c.paste(g2, "second via manager", Chord::CtrlV)?;
    expect_text(&got2, "second via manager", "second received")?;
    c.sleep_until(c.after_restore(at2));
    let after = c.probe()?;
    expect_text(&after, "orig mgr", "restored")?;
    Ok("manager-owned dictation recognised by text; original restored".into())
}

fn check_user_copy(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(&["--text", "text/plain;charset=utf-8=orig uc"], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    let (_, got, at) = c.paste(g, "dictation uc", Chord::CtrlV)?;
    expect_text(&got, "dictation uc", "received")?;
    std::thread::sleep(Duration::from_millis(1000));
    let _u = spawn_seed(&["--text", "text/plain;charset=utf-8=user copied this"], c.proto)?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "user copied this", "clipboard after the window")?;
    Ok("restore skipped: the user's newer copy is kept".into())
}

fn check_external_before_paste(c: &mut Ctx) -> Res<String> {
    let _s1 = spawn_seed(&["--text", "text/plain;charset=utf-8=orig x1"], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    std::thread::sleep(Duration::from_millis(200));
    let _s2 = spawn_seed(&["--text", "text/plain;charset=utf-8=orig x2"], c.proto)?;
    std::thread::sleep(Duration::from_millis(200));
    let (_, got, at) = c.paste(g, "dictation x", Chord::CtrlV)?;
    expect_text(&got, "dictation x", "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "orig x2", "restored")?;
    Ok("external change re-snapshotted; the newer content restored".into())
}

fn check_empty(c: &mut Ctx) -> Res<String> {
    run_cmd(exe("paste-spike").to_str().unwrap(), &["clear", "--proto", proto_name(c.proto)])?;
    std::thread::sleep(Duration::from_millis(100));
    let g = c.next_gen();
    c.snapshot(g);
    let (_, got, at) = c.paste(g, "dictation into empty", Chord::CtrlV)?;
    expect_text(&got, "dictation into empty", "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "dictation into empty", "clipboard after the window")?;
    Ok("empty clipboard: nothing to restore, the dictation stays".into())
}

fn check_oversize(c: &mut Ctx) -> Res<String> {
    let big = pseudo_random(11 * 1024 * 1024, 11);
    let path = sandbox_file("seed.bin", &big);
    let _s = spawn_seed(&["--file", &format!("application/octet-stream={path}")], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    std::thread::sleep(Duration::from_millis(300));
    let (_, got, at) = c.paste(g, "dictation big", Chord::CtrlV)?;
    expect_text(&got, "dictation big", "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "dictation big", "clipboard after the window")?;
    c.pump();
    if !c.noted(|n| matches!(n, Note::SnapshotFailed { error: ReadError::TooLarge, .. })) {
        return Err("expected SnapshotFailed TooLarge".into());
    }
    Ok("11 MiB selection: snapshot refused (>10 MB), no restore".into())
}

fn check_slow_source(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(&["--text", "text/plain;charset=utf-8=orig slow", "--delay-ms", "500"], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    let (timing, got, at) = c.paste(g, "dictation slow", Chord::CtrlV)?;
    expect_text(&got, "dictation slow", "received")?;
    let waited = (timing.sync2 - timing.ready) as f64 / 1000.0;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "dictation slow", "clipboard after the window")?;
    c.pump();
    if !c.noted(|n| matches!(n, Note::RestoreSkipped { reason, .. } if reason.contains("not ready"))) {
        return Err("expected restore to be skipped after the 100 ms wait".into());
    }
    Ok(format!("source answering after 500 ms: paste waited {waited:.0} ms, restore skipped"))
}

fn check_snapshot_timeout(c: &mut Ctx) -> Res<String> {
    let _s = spawn_seed(&["--text", "text/plain;charset=utf-8=orig never", "--delay-ms", "3000"], c.proto)?;
    let g = c.next_gen();
    c.snapshot(g);
    std::thread::sleep(Duration::from_millis(2300));
    let (_, got, at) = c.paste(g, "dictation timeout", Chord::CtrlV)?;
    expect_text(&got, "dictation timeout", "received")?;
    c.sleep_until(c.after_restore(at));
    let after = c.probe()?;
    expect_text(&after, "dictation timeout", "clipboard after the window")?;
    c.pump();
    if !c.noted(|n| matches!(n, Note::SnapshotFailed { error: ReadError::Timeout, .. })) {
        return Err("expected SnapshotFailed Timeout".into());
    }
    Ok("source silent for 3 s: snapshot read timed out at 2 s, paste went ahead without restore".into())
}

fn check_ctrl_shift_v(c: &mut Ctx) -> Res<String> {
    let g = c.next_gen();
    c.snapshot(g);
    let (_, got, _) = c.paste(g, "terminal paste", Chord::CtrlShiftV)?;
    expect_text(&got, "terminal paste", "received")?;
    if got["chord"] != "ctrl+shift+v" {
        return Err(format!("chord {}", got["chord"]));
    }
    Ok("Ctrl+Shift+V seen as ctrl+shift+v with keysym V".into())
}

/// Simulated physical keys on the headless host reach the nested niri as
/// real keyboard input, interpreted with niri's layout.
fn host_keys_run(seq: &str) -> Res<()> {
    let host = std::env::var("PSST_SANDBOX_HOST_SOCKET").map_err(|_| "PSST_SANDBOX_HOST_SOCKET unset")?;
    let o = Command::new(exe("paste-spike"))
        .args(["host-keys", seq])
        .env("WAYLAND_DISPLAY", &host)
        .env_remove("NIRI_SOCKET")
        .output()
        .map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(format!("host-keys: {}", String::from_utf8_lossy(&o.stderr)));
    }
    Ok(())
}

fn host_keys_spawn(seq: &str) -> Res<Child> {
    let host = std::env::var("PSST_SANDBOX_HOST_SOCKET").map_err(|_| "PSST_SANDBOX_HOST_SOCKET unset")?;
    Command::new(exe("paste-spike"))
        .args(["host-keys", seq])
        .env("WAYLAND_DISPLAY", &host)
        .env_remove("NIRI_SOCKET")
        .spawn()
        .map_err(|e| e.to_string())
}

fn check_layouts(c: &mut Ctx) -> Res<String> {
    let mut report = Vec::new();
    for (idx, name, two) in [(0, "us", "2"), (1, "cz-qwerty", "ě")] {
        run_cmd("niri", &["msg", "action", "switch-layout", &idx.to_string()])?;
        std::thread::sleep(Duration::from_millis(100));
        // A physical key before: niri's keymap and layout.
        c.t.drain();
        host_keys_run("tap:3")?;
        let k = c.t.wait("key", Duration::from_secs(3))?;
        if k["utf8"] != two {
            return Err(format!("{name}: physical KEY_2 before paste gave {} (want {two})", k["utf8"]));
        }
        let g = c.next_gen();
        c.snapshot(g);
        c.t.drain();
        let ready = now_us();
        c.p.send(Cmd::Paste {
            generation: g,
            text: format!("layout {name} ✓"),
            chord: Chord::CtrlV,
            delay_ms: 0,
            t_ready_us: ready,
        });
        let evs = c.t.collect_until("paste", Duration::from_secs(3))?;
        let got = evs.last().unwrap().clone();
        expect_text(&got, &format!("layout {name} ✓"), "received")?;
        let vk_keymap = evs.iter().any(|e| e["ev"] == "keymap" && e["name"] == "psst");
        // A physical key after: the real keymap must come back.
        std::thread::sleep(Duration::from_millis(100));
        c.t.drain();
        host_keys_run("tap:3")?;
        let evs = c.t.collect_until("key", Duration::from_secs(3))?;
        let k = evs.last().unwrap();
        if k["utf8"] != two {
            return Err(format!("{name}: physical KEY_2 after paste gave {} (want {two})", k["utf8"]));
        }
        let back = evs.iter().find(|e| e["ev"] == "keymap").map(|e| e["name"].clone()).unwrap_or(Value::Null);
        // Super held (physically) while pasting: paste only once the app has
        // seen Super go down; it is released 1.5 s later.
        c.t.drain();
        let mut hold = host_keys_spawn("press:125 sleep:1500 release:125")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let m = c.t.wait("mods", deadline.saturating_duration_since(Instant::now()))?;
            if m["logo"] == true {
                break;
            }
        }
        let g = c.next_gen();
        c.snapshot(g);
        let (_, got, _) = c.paste(g, &format!("super held {name}"), Chord::CtrlV)?;
        let _ = hold.wait();
        expect_text(&got, &format!("super held {name}"), "paste with Super held")?;
        if got["mods"]["ctrl"] != true || got["mods"]["logo"] != false {
            return Err(format!("{name}: with Super held the app saw mods {}", got["mods"]));
        }
        report.push(format!(
            "{name}: KEY_2→{two} before and after, vk keymap sent={vk_keymap}, real keymap back on next key ({back}), \
             Super held: paste received with mods {}",
            got["mods"],
        ));
    }
    run_cmd("niri", &["msg", "action", "switch-layout", "0"])?;
    Ok(report.join("; "))
}

// ---------------------------------------------------------------- bench

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn bench(a: &Args) -> Res<()> {
    let runs = a.num("runs", 300) as usize;
    let delay_ms = a.num("delay-ms", 0) as u32;
    let chord = a.chord();
    let opts = Options { proto: a.proto(), minimal_keymap: a.get("keymap") == Some("minimal"), ..Default::default() };
    let p = paster::spawn(opts)?;
    let t = Target::spawn()?;
    let proto = p.proto;
    let mut c = Ctx { p, t, proto, generation: 0, restore: Duration::from_secs(5), notes: vec![] };
    let mut cols: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    // With --physical, a simulated physical key precedes every paste (niri
    // only), so each paste switches the app to psst's keymap, as in real use.
    let physical = a.get("physical").is_some();
    for i in 0..runs {
        if physical {
            host_keys_run("tap:30")?;
            c.t.wait("key", Duration::from_secs(3))?;
        }
        let g = c.next_gen();
        c.snapshot(g);
        let text = format!("bench {i} ✓");
        c.t.drain();
        let ready = now_us();
        c.p.send(Cmd::Paste { generation: g, text: text.clone(), chord, delay_ms, t_ready_us: ready });
        let tm = c.wait_evt(Duration::from_secs(3), |e| match e {
            Evt::Pasted { generation, timing } if *generation == g => Some(timing.clone()),
            _ => None,
        })?;
        let got = c.t.wait("paste", Duration::from_secs(3))?;
        expect_text(&got, &text, "bench")?;
        let ms = |a: u64, b: u64| (b.saturating_sub(a)) as f64 / 1000.0;
        if i == 0 {
            println!(
                "# first paste: ready→Pasted {:.3} ms, ready→key {:.3} ms, ready→text {:.3} ms",
                ms(tm.ready, tm.sync2),
                ms(tm.ready, got["t_key"].as_u64().unwrap()),
                ms(tm.ready, got["t_done"].as_u64().unwrap())
            );
        }
        cols.entry("ready→Pasted (dispatch)").or_default().push(ms(tm.ready, tm.sync2));
        cols.entry("ready→thread got cmd").or_default().push(ms(tm.ready, tm.cmd));
        cols.entry("set_selection→sync").or_default().push(ms(tm.set_selection, tm.sync1));
        cols.entry("chord→sync").or_default().push(ms(tm.chord, tm.sync2));
        cols.entry("ready→key at app").or_default().push(ms(tm.ready, got["t_key"].as_u64().unwrap()));
        cols.entry("ready→text read by app").or_default().push(ms(tm.ready, got["t_done"].as_u64().unwrap()));
    }
    let compositor = std::env::var("PSST_SANDBOX_COMPOSITOR").unwrap_or_default();
    println!(
        "# bench: {runs} pastes, compositor={compositor}, data-control={}, chord={chord:?}, delay={delay_ms} ms, \
         keymap={}, physical key before each={physical}",
        proto_name(proto),
        a.get("keymap").unwrap_or("complete"),
    );
    println!("{:<28} {:>8} {:>8} {:>8} {:>8}", "ms", "median", "p90", "p99", "max");
    for (k, mut v) in cols {
        let (m, p90, p99) = (pct(&mut v, 0.5), pct(&mut v, 0.9), pct(&mut v, 0.99));
        let max = v.iter().cloned().fold(0.0, f64::max);
        println!("{k:<28} {m:>8.3} {p90:>8.3} {p99:>8.3} {max:>8.3}");
    }
    Ok(())
}

fn one_paste(a: &Args) -> Res<()> {
    let text = a.get("text").ok_or("--text is required")?.to_string();
    let p = paster::spawn(Options { proto: a.proto(), ..Default::default() })?;
    p.send(Cmd::Snapshot { generation: 1 });
    p.send(Cmd::Paste {
        generation: 1,
        text,
        chord: a.chord(),
        delay_ms: a.num("delay-ms", 0) as u32,
        t_ready_us: now_us(),
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    while let Ok(e) = p.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        println!("{e:?}");
        if matches!(e, Evt::Note(Note::Restored { .. } | Note::RestoreSkipped { .. })) {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- small clients

#[derive(Default)]
struct Simple {
    selection: Option<Option<DcOffer>>,
    cancelled: bool,
    delay_ms: u64,
}

impl DcHandler for Simple {
    fn dc_selection(&mut self, _: &Connection, offer: Option<DcOffer>) {
        if let Some(Some(old)) = self.selection.take() {
            old.destroy();
        }
        self.selection = Some(offer);
    }
    fn dc_send(&mut self, source: &DcSource, mime: String, fd: OwnedFd) {
        let Some(bytes) = source.data().bytes_for(&mime) else {
            return;
        };
        let delay = self.delay_ms;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            use std::io::Write;
            let _ = std::fs::File::from(fd).write_all((*bytes).as_ref());
        });
    }
    fn dc_cancelled(&mut self, source: &DcSource) {
        source.destroy();
        self.cancelled = true;
    }
}

wayland_paste::delegate_dc!(Simple);

impl Dispatch<WlRegistry, GlobalListContents> for Simple {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSeat, ()> for Simple {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for Simple {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for Simple {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

struct Client {
    conn: Connection,
    queue: wayland_client::EventQueue<Simple>,
    qh: QueueHandle<Simple>,
    globals: wayland_client::globals::GlobalList,
    seat: WlSeat,
    st: Simple,
}

fn client() -> Res<Client> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, queue) = registry_queue_init::<Simple>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).map_err(|e| e.to_string())?;
    Ok(Client { conn, queue, qh, globals, seat, st: Simple::default() })
}

fn dc_client(a: &Args) -> Res<(Client, DcManager, DcDevice)> {
    let mut c = client()?;
    let dc = DcManager::bind(&c.globals, &c.qh, a.proto())?;
    let dev = dc.get_device(&c.seat, &c.qh);
    c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    Ok((c, dc, dev))
}

/// `MIME=VALUE`; a MIME type may itself contain `=` (`text/plain;charset=utf-8`).
fn split_item(s: &str) -> Option<(&str, &str)> {
    for m in TEXT_MIMES {
        if let Some(v) = s.strip_prefix(m).and_then(|r| r.strip_prefix('=')) {
            return Some((m, v));
        }
    }
    s.split_once('=')
}

/// Own the clipboard with the given representations until replaced.
fn seed(a: &Args) -> Res<()> {
    let (mut c, dc, dev) = dc_client(a)?;
    c.st.delay_ms = a.num("delay-ms", 0);
    let mut items: Vec<(String, Bytes)> = Vec::new();
    for t in a.all("text") {
        let (m, v) = split_item(&t).ok_or("--text MIME=STR")?;
        items.push((m.to_string(), std::sync::Arc::new(v.as_bytes().to_vec())));
    }
    for f in a.all("file") {
        let (m, p) = split_item(&f).ok_or("--file MIME=PATH")?;
        items.push((m.to_string(), std::sync::Arc::new(std::fs::read(p).map_err(|e| e.to_string())?)));
    }
    let src = dc.create_source(&c.qh, items);
    dev.set_selection(Some(&src));
    c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    println!("ready");
    while !c.st.cancelled {
        c.queue.blocking_dispatch(&mut c.st).map_err(|e| e.to_string())?;
    }
    // Let in-flight writes finish.
    std::thread::sleep(Duration::from_millis(c.st.delay_ms + 50));
    Ok(())
}

fn clear(a: &Args) -> Res<()> {
    let (mut c, _dc, dev) = dc_client(a)?;
    dev.set_selection(None);
    c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    Ok(())
}

fn dump(a: &Args) -> Res<()> {
    let (mut c, _dc, _dev) = dc_client(a)?;
    let Some(Some(offer)) = c.st.selection.take() else {
        println!("(empty)");
        return Ok(());
    };
    let mimes = offer.mimes();
    println!("mimes: {mimes:?}");
    if let Some(m) = choose_mime(&mimes) {
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(|e| e.to_string())?;
        offer.receive(m.to_string(), std::os::fd::AsFd::as_fd(&w));
        drop(w);
        c.conn.flush().map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        std::fs::File::from(r).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if TEXT_MIMES.contains(&m) {
            println!("{m}: {:?}", String::from_utf8_lossy(&buf));
        } else {
            println!("{m}: {} bytes, fnv {}", buf.len(), fnv1a(&buf));
        }
    }
    Ok(())
}

/// Press keys on the headless host's seat (niri's winit window has focus).
fn host_keys(a: &Args) -> Res<()> {
    let seq = a.positional.first().ok_or("host-keys SEQ")?;
    let mut c = client()?;
    let vkm: ZwpVirtualKeyboardManagerV1 = c.globals.bind(&c.qh, 1..=1, ()).map_err(|e| e.to_string())?;
    let vk = vkm.create_virtual_keyboard(&c.seat, &c.qh, ());
    keymap::upload(&vk, keymap::HOST_KEYMAP).map_err(|e| e.to_string())?;
    c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    // The new keyboard capability makes niri's winit window bind wl_keyboard
    // and get an enter; keys sent before that are lost.
    std::thread::sleep(Duration::from_millis(300));
    let mut mods = 0u32;
    for step in seq.split_whitespace() {
        let (op, v) = step.split_once(':').ok_or("step op:value")?;
        let v: u32 = v.parse().map_err(|_| "number")?;
        let key = |code: u32, state: u32| vk.key(wayland_paste::clock::now_ms_u32(), code, state);
        match op {
            "press" => {
                key(v, 1);
                if v == keymap::KEY_LEFTMETA {
                    mods |= 1 << 6;
                    vk.modifiers(mods, 0, 0, 0);
                }
            }
            "release" => {
                key(v, 0);
                if v == keymap::KEY_LEFTMETA {
                    mods &= !(1 << 6);
                    vk.modifiers(mods, 0, 0, 0);
                }
            }
            "tap" => {
                key(v, 1);
                key(v, 0);
            }
            "sleep" => std::thread::sleep(Duration::from_millis(v as u64)),
            _ => return Err(format!("unknown step {op}")),
        }
        c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    }
    vk.destroy();
    c.queue.roundtrip(&mut c.st).map_err(|e| e.to_string())?;
    Ok(())
}
