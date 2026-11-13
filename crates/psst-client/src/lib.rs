//! The client side of `/v1/client`: a connection and its sessions, live and
//! upload. Used by the `psst` daemon and by `psst-eval`.
//!
//! The library reports problems and never retries or reconnects on its own; the daemon decides.
//!
//! # Shape
//!
//! [`Client::connect`] opens the WebSocket, exchanges `hello`/`welcome` and spawns one task that
//! owns the connection; a server on another protocol version refuses it with
//! [`ClientError::Incompatible`], which says the side to update. A [`Client`] is a cheap handle to that task, and so are the
//! [`Session`]s it starts and their [`FrameSender`]s. Everything a session does arrives through
//! [`Session::next_event`].
//!
//! - **One session at a time.** The protocol allows one active session per connection, so a
//!   `start` made while another session is active is sent once that session has ended.
//! - **Codec.** A session sends its audio in the [`AudioCodec`] its [`StartOpts`] ask for when the
//!   server's `welcome` lists it, and as PCM otherwise; [`Session::codec`] says which. Each
//!   session has an encoder of its own.
//! - **`progress` silence.** A session that hears nothing for [`PROGRESS_TIMEOUT`] after its
//!   `start` was sent, or after the last `ready`/`progress`, is [`SessionEvent::Lost`]. That ends
//!   the connection: every session on it is lost and [`Client::closed`] resolves.
//! - **Lifetime.** The connection stays open while a `Client`, a `Session`, a `FrameSender` or an
//!   upload still needs it, and while queued audio is still being sent. Dropping a session never
//!   cancels it; disconnecting doesn't cancel on the server either.
//!
//! [`api::ApiClient`] calls the HTTP API (`/api/*`) at the same endpoint.
//!
//! Everything here must run inside a tokio runtime.

pub mod api;
mod codec;
mod connection;
mod upload;
mod wav;

use std::{fmt, io, path::Path, sync::Arc, time::Duration};

use psst_proto::{
    ClientTimings, ErrorCode, FRAME_SAMPLES, HistoryId, PROTOCOL_VERSION, SessionId,
    ws::{
        Codec, ErrorMsg, EvalOpts, FinalMsg, Mode, ProgressState, ProtocolDetail, ReadyModelState, ReportMsg,
        WelcomeMsg,
    },
};
pub use psst_proto::{Endpoint, Token};
use tokio::sync::{mpsc, oneshot, watch};

pub use crate::codec::{AudioCodec, OpusApplication, OpusSettings};
pub use crate::wav::{WavError, WavInfo, read_wav_info};
use crate::{
    codec::Encoder,
    connection::{Cmd, Outgoing, Shared},
};

/// 20 ms of 16 kHz mono audio. The last frame of a recording is padded with zeros.
pub type AudioFrame = [i16; FRAME_SAMPLES];

/// Frames a [`FrameSender`] buffers: 30 s, so the backlog before `ready` never fills it.
pub const FRAME_BUFFER: usize = 1500;

/// Frames buffered between an upload's file reader and the connection: 1 s.
const UPLOAD_BUFFER: usize = 50;

/// How long `progress` may be silent before a session is [`SessionEvent::Lost`].
pub const PROGRESS_TIMEOUT: Duration = Duration::from_millis(1500);

/// How recently the server must have been heard from for [`Client::check_alive`] to skip the
/// ping.
pub const ALIVE_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to {endpoint}: {source}")]
    Connect { endpoint: Endpoint, source: io::Error },
    /// The server answered `hello` with an `error` other than `protocol`, for example
    /// `unauthorized`.
    #[error("the server refused the connection: {}", .0.message)]
    Refused(ErrorMsg),
    /// The server answered `hello` with `error protocol`: it speaks another protocol version, and
    /// one side must be updated.
    #[error(transparent)]
    Incompatible(Incompatible),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("no answer within the time budget")]
    Timeout,
    #[error("the connection is closed")]
    Closed,
}

/// The server speaks another protocol version than this client ([`PROTOCOL_VERSION`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}, this client (psst {}) protocol {PROTOCOL_VERSION}", self.server(), env!("CARGO_PKG_VERSION"))]
pub struct Incompatible {
    /// The protocol version the server speaks.
    pub server_protocol: u32,
    /// The server's version; servers from before [`ProtocolDetail`] don't say.
    pub server_version: Option<String>,
}

/// The side of the connection that has to be updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outdated {
    Client,
    Server,
}

impl Incompatible {
    /// From the `error protocol` that refused `hello`. Without a [`ProtocolDetail`], the server
    /// is from before it, and so speaks protocol 1.
    fn from_refusal(err: &ErrorMsg) -> Incompatible {
        let detail = err.detail.clone().and_then(|d| serde_json::from_value::<ProtocolDetail>(d).ok());
        match detail {
            Some(d) => Incompatible { server_protocol: d.protocol, server_version: Some(d.server_version) },
            None => Incompatible { server_protocol: 1, server_version: None },
        }
    }

    /// Which side speaks the older protocol, and so has to be updated.
    pub fn outdated(&self) -> Outdated {
        if self.server_protocol > PROTOCOL_VERSION { Outdated::Client } else { Outdated::Server }
    }

    fn server(&self) -> String {
        match &self.server_version {
            Some(version) => format!("the server (psst {version}) speaks protocol {}", self.server_protocol),
            None => format!("the server speaks protocol {}", self.server_protocol),
        }
    }
}

/// A connection that has completed `hello`/`welcome`.
///
/// A handle: clones share the connection, which stays open while any handle, session or queued
/// audio needs it, or until [`Client::close`].
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    welcome: Arc<WelcomeMsg>,
    cmd: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("endpoint", &self.endpoint).field("closed", &self.is_closed()).finish()
    }
}

impl Client {
    /// Connects and exchanges `hello`/`welcome`. `token` is required over TCP.
    ///
    /// There is no built-in time limit; callers wrap this in their own budget.
    pub async fn connect(ep: &Endpoint, token: Option<&Token>) -> Result<Client, ClientError> {
        connection::connect(ep, token).await
    }

    /// The server's `welcome`: its version, this device, the recording limit and the codecs.
    pub fn welcome(&self) -> &WelcomeMsg {
        &self.welcome
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The longest of the last four round trips measured on this connection: `hello`→`welcome`
    /// at connect, then each ping→pong of [`Client::check_alive`].
    pub fn round_trip(&self) -> Duration {
        self.shared.round_trip()
    }

    /// The check at `start` over TCP: if nothing arrived in the last 10 s, pings and
    /// expects a pong within `budget`.
    pub async fn check_alive(&mut self, budget: Duration) -> Result<(), ClientError> {
        if self.is_closed() {
            return Err(ClientError::Closed);
        }
        if self.shared.last_received().elapsed() < ALIVE_WINDOW {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Ping(tx)).map_err(|_| ClientError::Closed)?;
        match tokio::time::timeout(budget, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout),
        }
    }

    /// Sends `start` and returns at once; everything that follows arrives as [`SessionEvent`]s.
    ///
    /// If another session is still active on this connection, `start` is sent once it ends. If
    /// the encoder can't be set up, nothing is sent and the session ends with an `internal`
    /// [`SessionEvent::Error`].
    pub fn start(&mut self, opts: StartOpts) -> Session {
        self.start_with(opts, FRAME_BUFFER).0
    }

    fn start_with(&self, opts: StartOpts, buffer: usize) -> (Session, watch::Receiver<Option<bool>>) {
        let id = opts.session_id;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (frames_tx, frames_rx) = mpsc::channel(buffer);
        let (ready_tx, ready_rx) = watch::channel(None);
        let codec = match Encoder::new(opts.codec, &self.welcome.codecs) {
            Ok(encoder) => {
                let codec = encoder.codec();
                let start = Cmd::Start(Box::new(connection::NewSession {
                    opts,
                    events: events_tx,
                    frames: frames_rx,
                    ready: ready_tx,
                    encoder,
                }));
                // If the connection is gone, the command (and with it the event sender) is
                // dropped, so the session reports `Lost` at once.
                let _ = self.cmd.send(start);
                codec
            }
            Err(message) => {
                let _ = events_tx.send(SessionEvent::Error(ErrorMsg::new(Some(id), ErrorCode::Internal, message)));
                Codec::PcmS16le
            }
        };
        let session = Session {
            id,
            codec,
            events: events_rx,
            cmd: self.cmd.clone(),
            frames: FrameSender { tx: frames_tx },
            shared: self.shared.clone(),
            terminal: None,
        };
        (session, ready_rx)
    }

    /// Sends `report` for a finished session. Fire and forget: it gets no reply.
    pub fn report(&self, session_id: SessionId, history_id: HistoryId, timings: ClientTimings) {
        let _ = self.cmd.send(Cmd::Report(ReportMsg { session_id, history_id, timings }));
    }

    /// Closes the connection. Every session still on it is [`SessionEvent::Lost`].
    pub fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }

    /// Whether the connection has closed.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.borrow().is_some()
    }

    /// Resolves once the connection has closed, with the reason.
    pub async fn closed(&self) -> String {
        let mut rx = self.shared.closed.subscribe();
        match rx.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => "the connection closed".into(),
        }
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
    /// Used when the server accepts it; otherwise the audio goes as PCM.
    pub codec: AudioCodec,
}

/// One dictation on a connection. `Send`; all outcomes arrive through [`Session::next_event`].
pub struct Session {
    id: SessionId,
    codec: Codec,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    cmd: mpsc::UnboundedSender<Cmd>,
    frames: FrameSender,
    shared: Arc<Shared>,
    terminal: Option<SessionEvent>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("id", &self.id).field("terminal", &self.terminal).finish()
    }
}

impl Session {
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The codec its audio goes in: the one [`StartOpts::codec`] asked for if the server accepts
    /// it, else `pcm_s16le`.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The audio channel of a live session. Frames are sent once the server has answered
    /// `ready` with `need_audio`, oldest first; before that they wait in the buffer.
    pub fn frames(&self) -> FrameSender {
        self.frames.clone()
    }

    /// Sends `stop` once every frame given to the [`FrameSender`] has been sent. `samples` is the
    /// number of real (unpadded) samples.
    ///
    /// Returns [`ClientError::Closed`] if the session no longer takes audio: it ended, was
    /// cancelled, or the server needs none.
    pub async fn stop(&self, samples: u64) -> Result<(), ClientError> {
        self.frames.tx.send(Outgoing::Stop(samples)).await.map_err(|_| ClientError::Closed)
    }

    /// Sends `cancel`, in any state, including mid-upload. Audio not yet sent is dropped. A
    /// session whose `start` hasn't been sent yet ends with `Cancelled` without asking the
    /// server.
    pub async fn cancel(&self) -> Result<(), ClientError> {
        self.cmd.send(Cmd::Cancel(self.id)).map_err(|_| ClientError::Closed)
    }

    /// The next event. After a terminal event (`Final`, `Rejected`, `Error`, `Cancelled`,
    /// `Lost`) the session is over, and this returns the terminal event again.
    ///
    /// Cancel-safe: it can be used in `tokio::select!` without losing events.
    pub async fn next_event(&mut self) -> SessionEvent {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let event = match self.events.recv().await {
            Some(event) => event,
            None => SessionEvent::Lost(LostReason::ConnectionClosed { reason: self.shared.close_reason() }),
        };
        if event.is_terminal() {
            self.terminal = Some(event.clone());
        }
        event
    }
}

/// Feeds audio frames to a live session. `Clone + Send`.
#[derive(Debug, Clone)]
pub struct FrameSender {
    tx: mpsc::Sender<Outgoing>,
}

impl FrameSender {
    /// Queues a frame without ever blocking. Holds at least [`FRAME_BUFFER`] frames.
    pub fn try_send(&self, frame: AudioFrame) -> Result<(), TrySendError> {
        self.tx.try_send(Outgoing::Frame(Box::new(frame))).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => TrySendError::Full,
            mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
        })
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
    /// An `error` after `ready`. Also reported, with code `internal`, when an upload can't read
    /// its file or a frame can't be encoded; the session is then cancelled on the server.
    Error(ErrorMsg),
    Cancelled,
    Lost(LostReason),
}

impl SessionEvent {
    /// Whether the session is over after this event.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Ready { .. } | Self::Progress { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReason {
    /// No `progress` for [`PROGRESS_TIMEOUT`].
    ProgressTimeout,
    ConnectionClosed {
        reason: String,
    },
}

impl fmt::Display for LostReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProgressTimeout => write!(f, "no progress from the server for {PROGRESS_TIMEOUT:?}"),
            Self::ConnectionClosed { reason } => write!(f, "connection closed: {reason}"),
        }
    }
}

/// Uploads a finished recording. Returns at once; a task owned by the session waits for
/// `ready`, streams the file from disk with awaited backpressure (only if `need_audio`), then
/// sends `stop`. Dropping the session doesn't cancel it.
///
/// `wav` is a 16 kHz mono 16-bit WAV. Its length comes from the file size, so a header that was
/// never finalised doesn't matter.
pub fn upload_file(client: &mut Client, opts: StartOpts, wav: &Path) -> Session {
    let opts = StartOpts { mode: Mode::Upload, ..opts };
    let (session, ready) = client.start_with(opts, UPLOAD_BUFFER);
    tokio::spawn(upload::run(session.id, wav.to_path_buf(), session.frames.tx.clone(), ready, session.cmd.clone()));
    session
}
