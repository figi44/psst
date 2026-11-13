//! The client side of `/v1/client` (plan 5.1, 5.7): a connection and its sessions, live and
//! upload. Used by the `psst` daemon and by `psst-eval`.
//!
//! The library reports problems and never retries or reconnects on its own; the daemon decides.
//!
//! This is the contract stub: the types and signatures are final, the bodies arrive with the
//! client work package.

use std::{io, path::Path, time::Duration};

pub use psst_proto::{Endpoint, Token};
use psst_proto::{
    FRAME_SAMPLES, SessionId,
    ws::{ErrorMsg, EvalOpts, FinalMsg, Mode, ProgressState, ReadyModelState, WelcomeMsg},
};

/// 20 ms of 16 kHz mono audio. The last frame of a recording is padded with zeros.
pub type AudioFrame = [i16; FRAME_SAMPLES];

/// Frames a [`FrameSender`] buffers: 30 s, so the backlog before `ready` never fills it.
pub const FRAME_BUFFER: usize = 1500;

/// How long `progress` may be silent before a session is [`SessionEvent::Lost`].
pub const PROGRESS_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to {endpoint}: {source}")]
    Connect { endpoint: Endpoint, source: io::Error },
    /// The server answered `hello` with an `error` (for example `protocol` or `unauthorized`).
    #[error("the server refused the connection: {}", .0.message)]
    Refused(ErrorMsg),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("no answer within the time budget")]
    Timeout,
    #[error("the connection is closed")]
    Closed,
}

/// A connection that has completed `hello`/`welcome`.
#[derive(Debug)]
pub struct Client {
    _private: (),
}

impl Client {
    /// Connects and exchanges `hello`/`welcome`. `token` is required over TCP.
    pub async fn connect(ep: &Endpoint, token: Option<&Token>) -> Result<Client, ClientError> {
        let _ = (ep, token);
        todo!("psst-client: connect")
    }

    /// The server's `welcome`: its version, this device, and the recording limit.
    pub fn welcome(&self) -> &WelcomeMsg {
        todo!("psst-client: welcome")
    }

    /// The check at `start` over TCP (plan 5.1): if nothing arrived in the last 10 s, pings and
    /// expects a pong within `budget`.
    pub async fn check_alive(&mut self, budget: Duration) -> Result<(), ClientError> {
        let _ = budget;
        todo!("psst-client: check_alive")
    }

    /// Sends `start` and returns at once; everything that follows arrives as [`SessionEvent`]s.
    pub fn start(&mut self, opts: StartOpts) -> Session {
        let _ = opts;
        todo!("psst-client: start")
    }
}

/// What `start` sends. `upload_file` forces `mode` to [`Mode::Upload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOpts {
    pub session_id: SessionId,
    pub profile: Option<String>,
    pub mode: Mode,
    pub resume: bool,
    pub eval: Option<EvalOpts>,
}

/// One dictation on a connection. `Send`; all outcomes arrive through [`Session::next_event`].
#[derive(Debug)]
pub struct Session {
    _private: (),
}

impl Session {
    pub fn id(&self) -> SessionId {
        todo!("psst-client: Session::id")
    }

    /// The audio channel of a live session.
    pub fn frames(&self) -> FrameSender {
        todo!("psst-client: Session::frames")
    }

    /// Sends `stop` once every frame given to the [`FrameSender`] has been sent.
    pub async fn stop(&self, samples: u64) -> Result<(), ClientError> {
        let _ = samples;
        todo!("psst-client: Session::stop")
    }

    /// Sends `cancel`, in any state, including mid-upload.
    pub async fn cancel(&self) -> Result<(), ClientError> {
        todo!("psst-client: Session::cancel")
    }

    /// The next event. After a terminal event (`Final`, `Rejected`, `Error`, `Cancelled`,
    /// `Lost`) the session is over.
    pub async fn next_event(&mut self) -> SessionEvent {
        todo!("psst-client: Session::next_event")
    }
}

/// Feeds audio frames to a live session. `Clone + Send`.
#[derive(Debug, Clone)]
pub struct FrameSender {
    _private: (),
}

impl FrameSender {
    /// Queues a frame without ever blocking. Holds at least [`FRAME_BUFFER`] frames.
    pub fn try_send(&self, frame: AudioFrame) -> Result<(), TrySendError> {
        let _ = frame;
        todo!("psst-client: FrameSender::try_send")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrySendError {
    /// The buffer is full; the daemon treats the session as lost rather than drop audio.
    #[error("the frame buffer is full")]
    Full,
    #[error("the session is closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Ready {
        resumed: bool,
        need_audio: bool,
        model_state: ReadyModelState,
    },
    Progress {
        state: ProgressState,
        received_ms: u64,
    },
    Final(Box<FinalMsg>),
    /// An `error` before `ready` (`busy`, `model_unavailable`, `model_unloaded`, `bad_request`).
    Rejected(ErrorMsg),
    /// An `error` after `ready`.
    Error(ErrorMsg),
    Cancelled,
    Lost(LostReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReason {
    /// No `progress` for [`PROGRESS_TIMEOUT`].
    ProgressTimeout,
    ConnectionClosed {
        reason: String,
    },
}

/// Uploads a finished recording. Returns at once; a task owned by the session waits for
/// `ready`, streams the file from disk with awaited backpressure (only if `need_audio`), then
/// sends `stop`. Dropping the session doesn't cancel it.
pub fn upload_file(client: &mut Client, opts: StartOpts, wav: &Path) -> Session {
    let _ = (client, opts, wav);
    todo!("psst-client: upload_file")
}
