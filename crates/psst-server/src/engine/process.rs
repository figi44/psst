//! One engine child process: framed messages over its stdin and stdout, stderr forwarded to the
//! log, and its exit reported once.

use std::{io, process::Stdio, sync::Arc, time::Duration};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use psst_engine_proto::{
    DecodeError, FrameLimits, FromEngine, PROTOCOL_ENV, ToEngine, decode, encode, read_frame_async, write_frame_async,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};

use super::EngineConfig;

/// What the process tells the supervisor, tagged with its generation.
#[derive(Debug)]
pub enum ProcEvent {
    Message(Box<FromEngine>),
    /// A frame over a limit, or a stream that ended inside a frame.
    Corrupt(String),
    /// The process is gone; sent once, after its last message.
    Exited(String),
}

type Outgoing = (Vec<u8>, Arc<Vec<u8>>);

/// A running engine. Dropping it kills the process.
#[derive(Debug)]
pub struct EngineProcess {
    pub generation: u64,
    pid: Option<i32>,
    writer: mpsc::UnboundedSender<Outgoing>,
    kill: Option<oneshot::Sender<()>>,
}

impl EngineProcess {
    pub fn spawn(
        cfg: &EngineConfig,
        generation: u64,
        events: mpsc::UnboundedSender<(u64, ProcEvent)>,
    ) -> io::Result<EngineProcess> {
        let (program, args) = cfg.command.split_first().ok_or_else(|| io::Error::other("empty engine command"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .env(PROTOCOL_ENV, "1")
            .env("HF_HOME", &cfg.cache_dir)
            .env_remove("LISTEN_PID")
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_FDNAMES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: prctl is async-signal-safe. The engine dies with the server even when the server
        // is killed outright; systemd's control group covers the same case in production.
        unsafe {
            command.pre_exec(|| nix::sys::prctl::set_pdeathsig(Signal::SIGKILL).map_err(io::Error::from));
        }
        let mut child = command.spawn()?;
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (writer, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some((header, payload)) = outgoing.recv().await {
                if let Err(err) = write_frame_async(&mut stdin, &header, &payload).await {
                    tracing::debug!("engine stdin closed: {err}");
                    break;
                }
            }
        });

        let name = cfg.engine.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "psst_server::engine", "[{name}] {line}");
            }
        });

        let (read_done, reader_finished) = oneshot::channel::<()>();
        let reader_events = events.clone();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            let mut limits = FrameLimits::default();
            loop {
                match read_frame_async(&mut stdout, &limits).await {
                    Ok(Some(frame)) => match decode::<FromEngine>(&frame.header) {
                        Ok(msg) => {
                            if let FromEngine::Hello(hello) = &msg {
                                limits = FrameLimits::for_max_audio_s(hello.capabilities.max_audio_s);
                            }
                            let _ = reader_events.send((generation, ProcEvent::Message(Box::new(msg))));
                        }
                        Err(DecodeError::UnknownType(kind)) => {
                            tracing::warn!("ignoring an engine message of unknown type `{kind}`");
                        }
                        Err(err) => tracing::warn!("ignoring a malformed engine message: {err}"),
                    },
                    Ok(None) => break,
                    Err(err) => {
                        let _ = reader_events.send((generation, ProcEvent::Corrupt(err.to_string())));
                        break;
                    }
                }
            }
            let _ = read_done.send(());
        });

        let (kill, kill_requested) = oneshot::channel::<()>();
        tokio::spawn(async move {
            // A kill request, or the handle being dropped, kills the process.
            let status = tokio::select! {
                status = child.wait() => status,
                _ = kill_requested => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // Let the reader deliver what the engine wrote before it exited.
            let _ = tokio::time::timeout(Duration::from_secs(2), reader_finished).await;
            let status = match status {
                Ok(status) => status.to_string(),
                Err(err) => err.to_string(),
            };
            let _ = events.send((generation, ProcEvent::Exited(status)));
        });

        Ok(EngineProcess { generation, pid, writer, kill: Some(kill) })
    }

    pub fn send(&self, msg: &ToEngine, payload: Arc<Vec<u8>>) {
        let _ = self.writer.send((encode(msg), payload));
    }

    /// Asks the process to end (`SIGTERM`).
    pub fn terminate(&self) {
        if let Some(pid) = self.pid {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }

    /// Ends the process now (`SIGKILL`).
    pub fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}
