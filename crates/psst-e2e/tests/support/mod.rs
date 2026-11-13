//! Helpers shared by the Wayland end-to-end tests: the test app, and where the overlay is on a
//! screenshot.

#![allow(dead_code)] // each test file uses some of these

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use psst_testkit::{Sandbox, wayland::Screenshot};
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
