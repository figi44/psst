//! Reports problems and never retries or reconnects on its own; the daemon decides.

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

/// The last frame of a recording is padded with zeros.
pub type AudioFrame = [i16; FRAME_SAMPLES];

/// 30 s, so the backlog before `ready` never fills it.
pub const FRAME_BUFFER: usize = 1500;

const UPLOAD_BUFFER: usize = 50;

/// Server silence this long after `start`, or after the last message for the session, loses every
/// session and closes the connection.
pub const PROGRESS_TIMEOUT: Duration = Duration::from_millis(1500);

pub const ALIVE_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to {endpoint}: {source}")]
    Connect { endpoint: Endpoint, source: io::Error },
    #[error("the server refused the connection: {}", .0.message)]
    Refused(ErrorMsg),
    #[error(transparent)]
    Incompatible(Incompatible),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("no answer within the time budget")]
    Timeout,
    #[error("the connection is closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}, this client (psst {}) protocol {PROTOCOL_VERSION}", self.server(), env!("CARGO_PKG_VERSION"))]
pub struct Incompatible {
    pub server_protocol: u32,
    /// `None` from servers that predate [`ProtocolDetail`].
    pub server_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outdated {
    Client,
    Server,
}

impl Incompatible {
    /// Without a [`ProtocolDetail`], the server predates it, and so speaks protocol 1.
    fn from_refusal(err: &ErrorMsg) -> Incompatible {
        let detail = err.detail.clone().and_then(|d| serde_json::from_value::<ProtocolDetail>(d).ok());
        match detail {
            Some(d) => Incompatible { server_protocol: d.protocol, server_version: Some(d.server_version) },
            None => Incompatible { server_protocol: 1, server_version: None },
        }
    }

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

/// Clones share the connection, which stays open while any handle, session or queued audio needs
/// it, or until [`Client::close`].
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
    /// No built-in time limit; callers wrap this in their own budget.
    pub async fn connect(ep: &Endpoint, token: Option<&Token>) -> Result<Client, ClientError> {
        connection::connect(ep, token).await
    }

    pub fn welcome(&self) -> &WelcomeMsg {
        &self.welcome
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The longest of the last four round trips: `hello`→`welcome`, then each ping of
    /// [`Client::check_alive`].
    pub fn round_trip(&self) -> Duration {
        self.shared.round_trip()
    }

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

    /// The protocol allows one active session per connection, so while another is active,
    /// `start` waits until it ends.
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

    pub fn report(&self, session_id: SessionId, history_id: HistoryId, timings: ClientTimings) {
        let _ = self.cmd.send(Cmd::Report(ReportMsg { session_id, history_id, timings }));
    }

    pub fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.borrow().is_some()
    }

    pub async fn closed(&self) -> String {
        let mut rx = self.shared.closed.subscribe();
        match rx.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => "the connection closed".into(),
        }
    }
}

/// [`upload_file`] forces `mode` to [`Mode::Upload`].
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

/// Dropping it doesn't cancel it; nor does disconnecting.
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

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Frames wait in the buffer until `ready` with `need_audio`.
    pub fn frames(&self) -> FrameSender {
        self.frames.clone()
    }

    /// Goes out after every frame given to the [`FrameSender`]. `samples` counts the real
    /// (unpadded) samples. Also [`ClientError::Closed`] when the server needs no audio.
    pub async fn stop(&self, samples: u64) -> Result<(), ClientError> {
        self.frames.tx.send(Outgoing::Stop(samples)).await.map_err(|_| ClientError::Closed)
    }

    /// Audio not yet sent is dropped. A session whose `start` hasn't been sent yet ends without
    /// asking the server.
    pub async fn cancel(&self) -> Result<(), ClientError> {
        self.cmd.send(Cmd::Cancel(self.id)).map_err(|_| ClientError::Closed)
    }

    /// After a terminal event, returns it again. Cancel-safe.
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

#[derive(Debug, Clone)]
pub struct FrameSender {
    tx: mpsc::Sender<Outgoing>,
}

impl FrameSender {
    /// Waits while the buffer is full. [`ClientError::Closed`] once the session takes no more audio.
    pub async fn send(&self, frame: AudioFrame) -> Result<(), ClientError> {
        self.tx.send(Outgoing::Frame(Box::new(frame))).await.map_err(|_| ClientError::Closed)
    }

    pub fn try_send(&self, frame: AudioFrame) -> Result<(), TrySendError> {
        self.tx.try_send(Outgoing::Frame(Box::new(frame))).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => TrySendError::Full,
            mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrySendError {
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
    /// An `error` before `ready`.
    Rejected(ErrorMsg),
    /// An `error` after `ready`. Also `internal` when an upload can't read its file or a frame
    /// can't be encoded; the session is then cancelled on the server.
    Error(ErrorMsg),
    Cancelled,
    Lost(LostReason),
}

impl SessionEvent {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Ready { .. } | Self::Progress { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReason {
    ProgressTimeout,
    ConnectionClosed { reason: String },
}

impl fmt::Display for LostReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProgressTimeout => write!(f, "no progress from the server for {PROGRESS_TIMEOUT:?}"),
            Self::ConnectionClosed { reason } => write!(f, "connection closed: {reason}"),
        }
    }
}

/// `wav` is a 16 kHz mono 16-bit WAV. Its length comes from the file size, so a header that was
/// never finalised doesn't matter.
pub fn upload_file(client: &mut Client, opts: StartOpts, wav: &Path) -> Session {
    let opts = StartOpts { mode: Mode::Upload, ..opts };
    let (session, ready) = client.start_with(opts, UPLOAD_BUFFER);
    tokio::spawn(upload::run(session.id, wav.to_path_buf(), session.frames.tx.clone(), ready, session.cmd.clone()));
    session
}
