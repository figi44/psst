use std::{
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    process::ExitCode,
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde::Serialize;

use crate::ipc::{CLI_TIMEOUT, Request, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Waybar,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { first: Duration::from_millis(250), max: Duration::from_secs(5) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaybarLine {
    pub text: String,
    pub alt: &'static str,
    pub class: Vec<&'static str>,
    /// Already-escaped Pango markup, so waybar's `escape` must stay off.
    pub tooltip: String,
}

impl WaybarLine {
    pub fn new(status: &Status) -> WaybarLine {
        let state = status.state.as_str();
        let (alt, headline) = if state == "stopped" {
            ("stopped", "the daemon isn't running".to_string())
        } else if let Some(recording) = &status.recording {
            // Also while the previous dictation is still being inserted.
            match recording.offline.as_deref() {
                Some(reason) => ("offline", format!("recording offline, sent at stop ({reason})")),
                None => ("recording", "recording".to_string()),
            }
        } else if let Some(error) = &status.error {
            ("error", error.clone())
        } else if matches!(state, "finishing" | "inserting") {
            let phase = status.finishing.first().map_or(state, |d| d.phase.as_str());
            ("finishing", phase.to_string())
        } else {
            ("idle", "idle".to_string())
        };
        let saved = status.saved();
        let mut class = Vec::new();
        if alt == "offline" {
            class.push("recording");
        }
        class.push(alt);
        if saved > 0 {
            class.push("saved");
        }
        if !status.warnings.is_empty() {
            class.push("warning");
        }
        let mut tooltip = format!("psst: {headline}");
        if alt != "stopped" {
            tooltip += &format!("\nserver: {}", status.connection);
        }
        if saved > 0 {
            let s = if saved == 1 { "" } else { "s" };
            tooltip += &format!("\n{saved} saved recording{s}: see psst status");
        }
        for warning in &status.warnings {
            tooltip += &format!("\nwarning: {warning}");
        }
        WaybarLine {
            text: if saved > 0 { saved.to_string() } else { String::new() },
            alt,
            class,
            tooltip: escape_markup(&tooltip),
        }
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn render(format: Format, status: &Status) -> String {
    match format {
        Format::Waybar => serde_json::to_string(&WaybarLine::new(status)),
        Format::Json => serde_json::to_string(status),
    }
    .expect("status lines serialise")
}

pub fn main(json: bool) -> anyhow::Result<ExitCode> {
    let socket = crate::socket_path()?;
    let format = if json { Format::Json } else { Format::Waybar };
    run(&socket, format, &mut io::stdout().lock(), Backoff::default())?;
    Ok(ExitCode::SUCCESS)
}

pub fn run<W: Write + AsFd>(socket: &Path, format: Format, out: &mut W, backoff: Backoff) -> io::Result<()> {
    let mut printer = Printer { out, format, last: None };
    let mut delay = backoff.first;
    loop {
        let started = Instant::now();
        let delivered = match UnixStream::connect(socket) {
            Ok(stream) => match follow(stream, &mut printer)? {
                Ended::OutputClosed => return Ok(()),
                Ended::Disconnected { delivered } => delivered,
            },
            Err(_) => false,
        };
        if delivered {
            // The daemon was there: try again at once, unless it keeps hanging up straight away.
            delay = backoff.first;
            if started.elapsed() < backoff.first && output_closed_within(&*printer.out, backoff.first)? {
                return Ok(());
            }
            continue;
        }
        if !printer.print(&Status::stopped())? || output_closed_within(&*printer.out, delay)? {
            return Ok(());
        }
        delay = (delay * 2).min(backoff.max);
    }
}

struct Printer<'a, W> {
    out: &'a mut W,
    format: Format,
    last: Option<String>,
}

impl<W: Write> Printer<'_, W> {
    fn print(&mut self, status: &Status) -> io::Result<bool> {
        let line = render(self.format, status);
        if self.last.as_ref() == Some(&line) {
            return Ok(true);
        }
        match writeln!(self.out, "{line}").and_then(|()| self.out.flush()) {
            Ok(()) => {
                self.last = Some(line);
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
            Err(err) => Err(err),
        }
    }
}

enum Ended {
    OutputClosed,
    Disconnected { delivered: bool },
}

fn follow<W: Write + AsFd>(mut stream: UnixStream, printer: &mut Printer<W>) -> io::Result<Ended> {
    let mut request = serde_json::to_string(&Request::Follow).expect("requests serialise");
    request.push('\n');
    stream.set_write_timeout(Some(CLI_TIMEOUT))?;
    if stream.write_all(request.as_bytes()).is_err() {
        return Ok(Ended::Disconnected { delivered: false });
    }
    let mut delivered = false;
    let mut pending = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        // Polling the output too ends this when the bar goes away, even while nothing changes.
        let (readable, closed) = {
            let mut fds = [PollFd::new(&stream, PollFlags::IN), PollFd::new(&*printer.out, PollFlags::empty())];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(err.into()),
            }
            (!fds[0].revents().is_empty(), is_closed(fds[1].revents()))
        };
        if closed {
            return Ok(Ended::OutputClosed);
        }
        if !readable {
            continue;
        }
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            return Ok(Ended::Disconnected { delivered });
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            match serde_json::from_slice::<Status>(&line) {
                Ok(status) => {
                    delivered = true;
                    if !printer.print(&status)? {
                        return Ok(Ended::OutputClosed);
                    }
                }
                // For example an older daemon refusing `follow`.
                Err(_) => return Ok(Ended::Disconnected { delivered: false }),
            }
        }
    }
}

fn output_closed_within(out: &impl AsFd, wait: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let timeout = Timespec::try_from(left).unwrap_or(Timespec { tv_sec: 1, tv_nsec: 0 });
        let mut fds = [PollFd::new(out, PollFlags::empty())];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) if is_closed(fds[0].revents()) => return Ok(true),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// A pipe with no reader reports `ERR`, a hung-up socket or terminal `HUP`, a closed fd `NVAL`.
fn is_closed(revents: PollFlags) -> bool {
    revents.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
}
