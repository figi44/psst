//! Helpers shared by the Wayland end-to-end tests: the test app, where the overlay is on a
//! screenshot and what it shows, and a proxy that can cut the daemon's connection to the server.

#![allow(dead_code)] // each test file uses some of these

use std::{
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use psst_testkit::{
    Sandbox,
    wayland::{Screenshot, screenshot},
};
use serde_json::Value;

pub const TARGET: &str = env!("CARGO_BIN_EXE_psst-paste-target");

// ---------------------------------------------------------------------- the app

/// The test app: it has keyboard focus and reports what it pastes.
pub struct App {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
}

impl App {
    /// Starts the app and waits until it has keyboard focus. On headless sway, start psst's
    /// Wayland thread first: its virtual keyboard is the seat's only keyboard.
    pub fn start(sandbox: &Sandbox) -> Self {
        let mut child = sandbox
            .command(TARGET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start psst-paste-target");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let app = Self { child, stdin, rx };
        app.wait("focus", Duration::from_secs(10));
        app
    }

    /// The next event named `ev`; others are dropped.
    pub fn wait(&self, ev: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(v) if v["ev"] == ev => return v,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("the app sent no {ev} event within {timeout:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("the app exited"),
            }
        }
    }

    /// Every event up to and including the next `ev`.
    pub fn until(&self, ev: &str, timeout: Duration) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        loop {
            let v = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|_| panic!("the app sent no {ev} event within {timeout:?}"));
            let done = v["ev"] == ev;
            events.push(v);
            if done {
                return events;
            }
        }
    }

    /// The events that arrived and haven't been waited for.
    pub fn pending(&self) -> Vec<Value> {
        self.rx.try_iter().collect()
    }

    pub fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// The paste the app received.
    pub fn pasted(&self) -> Value {
        self.wait("paste", Duration::from_secs(3))
    }

    /// What the app would paste now.
    pub fn probe(&mut self) -> Value {
        self.drain();
        writeln!(self.stdin, "probe").expect("write to the app");
        let v = self.pasted();
        assert_eq!(v["chord"], "probe");
        v
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------- the overlay

/// Where the pill is: the bounding box of pixels that differ from the background.
pub fn pill_bounds(shot: &Screenshot) -> Option<(u32, u32, u32, u32)> {
    let background = shot.pixel(5, 5);
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for y in 0..shot.height {
        for x in 0..shot.width {
            let p = shot.pixel(x, y);
            if p[..3].iter().zip(&background[..3]).any(|(a, b)| a.abs_diff(*b) > 24) {
                let b = bounds.get_or_insert((x, y, x, y));
                *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            }
        }
    }
    bounds
}

/// What the overlay shows, told apart by the pixel in the middle of its icon well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Hidden,
    /// `Starting`: an accent ring, nothing in the middle.
    Starting,
    /// `Recording`: the accent dot.
    Recording,
    /// `Offline`: an amber dot.
    Offline,
    /// `Processing`, `Sending`, `WaitingForServer`, `LoadingModel`, `Retrying`: a light icon.
    Busy,
    Done,
    Error,
    /// A pill that is none of the above (`Flash`, `NothingHeard`, …).
    Other,
}

/// Where the overlay is on `shot`: the bounding box of pixels that differ from `base`, the same
/// screen without it. Only the bottom half's middle is searched, where the default `[overlay]`
/// puts it.
pub fn overlay_bounds(base: &Screenshot, shot: &Screenshot) -> Option<(u32, u32, u32, u32)> {
    let (width, height) = (shot.width.min(base.width), shot.height.min(base.height));
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for y in height / 2..height {
        for x in width / 4..width * 3 / 4 {
            let (p, q) = (shot.pixel(x, y), base.pixel(x, y));
            if p[..3].iter().zip(&q[..3]).any(|(a, b)| a.abs_diff(*b) > 24) {
                let b = bounds.get_or_insert((x, y, x, y));
                *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
            }
        }
    }
    bounds
}

/// What the overlay shows on `shot`, with the default `[overlay]` look; `base` is the same screen
/// without the overlay.
pub fn seen(base: &Screenshot, shot: &Screenshot) -> Seen {
    let Some((x0, y0, _, y1)) = overlay_bounds(base, shot) else { return Seen::Hidden };
    let h = y1 - y0 + 1;
    let [r, g, b, _] = shot.pixel(x0 + h / 2, y0 + h / 2);
    let near = |want: [u8; 3], tolerance: u8| [r, g, b].iter().zip(want).all(|(a, w)| a.abs_diff(w) <= tolerance);
    if near([0xff, 0x45, 0x3a], 12) {
        Seen::Recording
    } else if near([0xff, 0xb0, 0x3b], 12) {
        Seen::Offline
    } else if near([0xff, 0x5f, 0x52], 12) {
        Seen::Error
    } else if g > r.saturating_add(60) && g > b.saturating_add(30) {
        Seen::Done
    } else if r.min(g).min(b) >= 150 && r.max(g).max(b) - r.min(g).min(b) <= 24 {
        Seen::Busy
    } else if r.max(g).max(b) <= 80 {
        Seen::Starting
    } else {
        Seen::Other
    }
}

/// Watches the screen from a thread and keeps each change of what the overlay shows, with when
/// the screenshot that first showed it was asked for. The first screenshot is the screen without the overlay, which every
/// later one is compared with, so nothing but the overlay may change while it watches.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<(Seen, Instant)>>>,
    /// How far [`Watcher::wait_for`] has got in `seen`.
    cursor: AtomicUsize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
    /// Takes a screenshot every `every` until dropped or [`Watcher::stop`]ped. Start it while
    /// the overlay is hidden.
    pub fn start(sandbox: Arc<Sandbox>, every: Duration) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let seen: Arc<Mutex<Vec<(Seen, Instant)>>> = Arc::default();
        let (thread_stop, thread_seen) = (stop.clone(), seen.clone());
        let thread = thread::spawn(move || {
            let base = screenshot(&sandbox);
            while !thread_stop.load(Ordering::Acquire) {
                let asked = Instant::now();
                let now = self::seen(&base, &screenshot(&sandbox));
                let mut log = thread_seen.lock().unwrap();
                if log.last().map(|(s, _)| *s) != Some(now) {
                    log.push((now, asked));
                }
                drop(log);
                thread::sleep(every);
            }
        });
        Watcher { stop, seen, cursor: AtomicUsize::new(0), thread: Some(thread) }
    }

    /// What has been seen so far, in order, without repeats.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().iter().map(|(s, _)| *s).collect()
    }

    /// Waits until the overlay shows `want` after what earlier calls waited for, and returns
    /// when it was first seen; panics with what was seen after `timeout`.
    #[track_caller]
    pub fn wait_for(&self, want: Seen, timeout: Duration) -> Instant {
        let deadline = Instant::now() + timeout;
        loop {
            let from = self.cursor.load(Ordering::Acquire);
            let found = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .skip(from)
                .find(|(_, (s, _))| *s == want)
                .map(|(i, (_, at))| (i, *at));
            if let Some((i, at)) = found {
                self.cursor.store(i + 1, Ordering::Release);
                return at;
            }
            assert!(Instant::now() < deadline, "the overlay never showed {want:?}; it showed {:?}", self.seen());
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn stop(&mut self) -> Vec<Seen> {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the watcher thread");
        }
        self.seen()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether `want` appears in `seen` in this order (other states may come in between).
pub fn in_order(seen: &[Seen], want: &[Seen]) -> bool {
    let mut it = seen.iter();
    want.iter().all(|w| it.any(|s| s == w))
}

// ---------------------------------------------------------------------- the network

/// A Unix-socket proxy in front of the server, so a test can cut the daemon's connection as a
/// network failure would while the server keeps running.
pub struct Proxy {
    path: PathBuf,
    open: Arc<Mutex<Vec<UnixStream>>>,
}

impl Proxy {
    /// Listens on `path` and forwards each connection to `upstream`.
    pub fn start(path: &Path, upstream: &Path) -> Proxy {
        let listener = UnixListener::bind(path).unwrap_or_else(|err| panic!("bind {}: {err}", path.display()));
        let open: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
        let (accepted, upstream) = (open.clone(), upstream.to_path_buf());
        thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                // The server is down: the daemon sees the connection close at once.
                let Ok(server) = UnixStream::connect(&upstream) else { continue };
                let mut open = accepted.lock().unwrap();
                open.extend([client.try_clone().unwrap(), server.try_clone().unwrap()]);
                drop(open);
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Proxy { path: path.to_path_buf(), open }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes every connection open now; new ones still go through.
    pub fn cut(&self) {
        for stream in self.open.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn pipe(mut from: UnixStream, mut to: UnixStream) {
    thread::spawn(move || {
        let _ = io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}
