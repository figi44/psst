//! The task that owns one WebSocket connection: it writes what the handles ask for, routes what
//! the server sends to the active session, and watches for `progress` silence.

use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use psst_proto::{
    Endpoint, FRAME_BYTES, PROTOCOL_VERSION, SessionId, Token,
    ws::{AudioFormat, CancelMsg, ClientInfo, ClientMsg, ErrorMsg, HelloMsg, ReportMsg, ServerMsg, StartMsg, StopMsg},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpStream, UnixStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, client::IntoClientRequest, http::HeaderValue},
};
use tracing::{debug, warn};

use crate::{AudioFrame, Client, ClientError, LostReason, PROGRESS_TIMEOUT, SessionEvent, StartOpts};

/// Frames written before one flush during an upload.
const WRITE_BATCH: usize = 50;

pub(crate) enum Cmd {
    Start(Box<NewSession>),
    Cancel(SessionId),
    /// The upload task couldn't read its file: end the session with this error and cancel it on
    /// the server.
    Fail(SessionId, String),
    Report(ReportMsg),
    Ping(oneshot::Sender<()>),
    Close,
}

pub(crate) struct NewSession {
    pub opts: StartOpts,
    pub events: mpsc::UnboundedSender<SessionEvent>,
    pub frames: mpsc::Receiver<Outgoing>,
    /// `Some(need_audio)` once `ready` arrived; closed when the session ends.
    pub ready: watch::Sender<Option<bool>>,
}

pub(crate) enum Outgoing {
    Frame(Box<AudioFrame>),
    Stop(u64),
}

/// State the handles read without asking the task.
pub(crate) struct Shared {
    last_received: Mutex<Instant>,
    /// `Some(reason)` once the connection has closed.
    pub closed: watch::Sender<Option<String>>,
}

impl Shared {
    pub fn last_received(&self) -> Instant {
        *self.last_received.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn heard(&self, at: Instant) {
        *self.last_received.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    pub fn close_reason(&self) -> String {
        self.closed.borrow().clone().unwrap_or_else(|| "the connection closed".into())
    }
}

/// A Unix or TCP stream.
enum Transport {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl AsyncRead for Transport {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

type Ws = WebSocketStream<Transport>;

pub(crate) async fn connect(ep: &Endpoint, token: Option<&Token>) -> Result<Client, ClientError> {
    let connect_err = |source| ClientError::Connect { endpoint: ep.clone(), source };
    let (transport, authority) = match ep {
        Endpoint::Unix(path) => {
            (Transport::Unix(UnixStream::connect(path).await.map_err(connect_err)?), "localhost".into())
        }
        Endpoint::Tcp { host, port } => {
            let stream = TcpStream::connect((host.as_str(), *port)).await.map_err(connect_err)?;
            stream.set_nodelay(true).map_err(connect_err)?;
            let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
            (Transport::Tcp(stream), authority)
        }
    };
    let mut request = format!("ws://{authority}{}", psst_proto::api::routes::CLIENT)
        .into_client_request()
        .map_err(|err| ClientError::Protocol(err.to_string()))?;
    if let Some(token) = token {
        let value = HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
            .map_err(|_| ClientError::Protocol("the token is not a valid header value".into()))?;
        request.headers_mut().insert("authorization", value);
    }
    let (mut ws, _) = tokio_tungstenite::client_async(request, transport).await.map_err(|err| match err {
        tungstenite::Error::Io(source) => connect_err(source),
        other => ClientError::Protocol(format!("WebSocket handshake failed: {other}")),
    })?;

    let hello = ClientMsg::Hello(HelloMsg {
        protocol: PROTOCOL_VERSION,
        client: ClientInfo { name: "psst".into(), version: env!("CARGO_PKG_VERSION").into() },
    });
    ws.send(text(&hello)).await.map_err(ws_err)?;
    let welcome = loop {
        let msg = ws.next().await.ok_or(ClientError::Closed)?.map_err(ws_err)?;
        match msg {
            Message::Text(body) => match serde_json::from_str::<ServerMsg>(&body) {
                Ok(ServerMsg::Welcome(welcome)) => break welcome,
                Ok(ServerMsg::Error(err)) => return Err(ClientError::Refused(err)),
                Ok(other) => return Err(ClientError::Protocol(format!("expected welcome, got {other:?}"))),
                Err(err) => return Err(ClientError::Protocol(format!("expected welcome: {err}"))),
            },
            Message::Close(_) => return Err(ClientError::Closed),
            _ => {}
        }
    };

    let (closed, _) = watch::channel(None);
    let shared = Arc::new(Shared { last_received: Mutex::new(Instant::now()), closed });
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (sink, mut stream) = ws.split();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let reader_shared = shared.clone();
    let reader = tokio::spawn(async move {
        let reason = loop {
            match stream.next().await {
                Some(Ok(msg)) => {
                    let at = Instant::now();
                    reader_shared.heard(at);
                    if incoming_tx.send(Incoming::Msg(msg, at)).is_err() {
                        return;
                    }
                }
                Some(Err(err)) => break err.to_string(),
                None => break "the server closed the connection".into(),
            }
        };
        let _ = incoming_tx.send(Incoming::Closed(reason));
    });
    let actor = Actor {
        sink,
        incoming: incoming_rx,
        cmds: cmd_rx,
        cmds_open: true,
        active: None,
        queued: VecDeque::new(),
        pings: Vec::new(),
        shared: shared.clone(),
        connection_error: None,
        reader,
    };
    tokio::spawn(actor.run());
    Ok(Client { endpoint: ep.clone(), welcome: Arc::new(welcome), cmd: cmd_tx, shared })
}

fn ws_err(err: tungstenite::Error) -> ClientError {
    match err {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => ClientError::Closed,
        other => ClientError::Protocol(other.to_string()),
    }
}

fn text(msg: &ClientMsg) -> Message {
    Message::text(serde_json::to_string(msg).expect("client messages serialise"))
}

enum Incoming {
    Msg(Message, Instant),
    Closed(String),
}

struct Active {
    id: SessionId,
    events: mpsc::UnboundedSender<SessionEvent>,
    /// Audio still to send; `None` once `stop` went out, or the session needs no (more) audio.
    frames: Option<mpsc::Receiver<Outgoing>>,
    /// Whether `ready` arrived with `need_audio`, so `frames` may be sent.
    streaming: bool,
    ready: Option<watch::Sender<Option<bool>>>,
    got_ready: bool,
    /// The session already ended for its owner (cancelled, or failed locally); the slot stays
    /// taken until the server's terminal message.
    ended: bool,
    /// When the server was last heard from for this session (or when `start` went out).
    last_heard: Instant,
}

struct Actor {
    sink: SplitSink<Ws, Message>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    cmds_open: bool,
    active: Option<Active>,
    queued: VecDeque<NewSession>,
    pings: Vec<oneshot::Sender<()>>,
    shared: Arc<Shared>,
    /// An `error` without a session id, kept as the reason if the connection then closes.
    connection_error: Option<ErrorMsg>,
    reader: JoinHandle<()>,
}

enum Flow {
    Continue,
    Close(String),
}

impl Actor {
    async fn run(mut self) {
        let reason = loop {
            if !self.cmds_open && !self.has_pending_audio() && self.queued.is_empty() {
                break "no handle uses the connection any more".to_string();
            }
            let deadline = self.active.as_ref().map(|a| a.last_heard + PROGRESS_TIMEOUT);
            let flow = tokio::select! {
                biased;
                incoming = self.incoming.recv() => match incoming {
                    Some(Incoming::Msg(msg, at)) => self.on_message(msg, at).await,
                    Some(Incoming::Closed(reason)) => Flow::Close(self.closed_reason(reason)),
                    None => Flow::Close("the reader stopped".into()),
                },
                cmd = self.cmds.recv(), if self.cmds_open => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => {
                        self.cmds_open = false;
                        Flow::Continue
                    }
                },
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.lose_all(LostReason::ProgressTimeout);
                    Flow::Close(LostReason::ProgressTimeout.to_string())
                }
                out = next_outgoing(&mut self.active) => self.on_outgoing(out).await,
            };
            if let Flow::Close(reason) = flow {
                break reason;
            }
        };
        debug!("closing the connection: {reason}");
        let _ = self.sink.send(Message::Close(None)).await;
        let _ = self.sink.close().await;
        self.reader.abort();
        self.lose_all(LostReason::ConnectionClosed { reason: reason.clone() });
        self.shared.closed.send_replace(Some(reason));
        // Pending pings fail when their senders drop with `self`.
    }

    fn closed_reason(&self, reason: String) -> String {
        match &self.connection_error {
            Some(err) => format!("{}: {}", serde_json::to_string(&err.code).unwrap_or_default(), err.message),
            None => reason,
        }
    }

    fn has_pending_audio(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.frames.is_some())
    }

    /// Ends the active and queued sessions with `Lost`.
    fn lose_all(&mut self, reason: LostReason) {
        if let Some(active) = self.active.take()
            && !active.ended
        {
            let _ = active.events.send(SessionEvent::Lost(reason.clone()));
        }
        for queued in self.queued.drain(..) {
            let _ = queued.events.send(SessionEvent::Lost(reason.clone()));
        }
    }

    async fn write(&mut self, msg: Message) -> Flow {
        match self.sink.send(msg).await {
            Ok(()) => Flow::Continue,
            Err(err) => Flow::Close(format!("cannot write: {err}")),
        }
    }

    async fn on_cmd(&mut self, cmd: Cmd) -> Flow {
        match cmd {
            Cmd::Start(new) => {
                if self.active.is_some() {
                    self.queued.push_back(*new);
                    Flow::Continue
                } else {
                    self.send_start(*new).await
                }
            }
            Cmd::Cancel(id) => self.cancel(id, None).await,
            Cmd::Fail(id, reason) => self.cancel(id, Some(reason)).await,
            Cmd::Report(report) => self.write(text(&ClientMsg::Report(report))).await,
            Cmd::Ping(tx) => {
                let first = self.pings.is_empty();
                self.pings.push(tx);
                if first { self.write(Message::Ping(Default::default())).await } else { Flow::Continue }
            }
            Cmd::Close => Flow::Close("closed by the client".into()),
        }
    }

    async fn send_start(&mut self, new: NewSession) -> Flow {
        let NewSession { opts, events, frames, ready } = new;
        let start = ClientMsg::Start(StartMsg {
            session_id: opts.session_id,
            profile: opts.profile,
            audio: AudioFormat::default(),
            mode: opts.mode,
            resume: opts.resume,
            eval: opts.eval,
        });
        self.active = Some(Active {
            id: opts.session_id,
            events,
            frames: Some(frames),
            streaming: false,
            ready: Some(ready),
            got_ready: false,
            ended: false,
            last_heard: Instant::now(),
        });
        let flow = self.write(text(&start)).await;
        if let Some(active) = &mut self.active {
            active.last_heard = Instant::now();
        }
        flow
    }

    /// Cancels a session: on the server if its `start` went out, locally if it's still queued.
    /// With `failure`, the owner sees that error instead of `Cancelled`.
    async fn cancel(&mut self, id: SessionId, failure: Option<String>) -> Flow {
        let ending = |failure: Option<String>| match failure {
            Some(message) => SessionEvent::Error(ErrorMsg::new(Some(id), psst_proto::ErrorCode::Internal, message)),
            None => SessionEvent::Cancelled,
        };
        if let Some(pos) = self.queued.iter().position(|q| q.opts.session_id == id) {
            let queued = self.queued.remove(pos).expect("position is in range");
            let _ = queued.events.send(ending(failure));
            return Flow::Continue;
        }
        let Some(active) = self.active.as_mut().filter(|a| a.id == id) else {
            return Flow::Continue; // a late cancel
        };
        active.frames = None;
        active.streaming = false;
        if failure.is_some() && !active.ended {
            active.ended = true;
            let _ = active.events.send(ending(failure));
        }
        self.write(text(&ClientMsg::Cancel(CancelMsg { session_id: id }))).await
    }

    async fn on_message(&mut self, msg: Message, at: Instant) -> Flow {
        if !self.pings.is_empty() {
            for ping in self.pings.drain(..) {
                let _ = ping.send(());
            }
        }
        let body = match msg {
            Message::Text(body) => body,
            Message::Close(frame) => {
                let reason = frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty());
                return Flow::Close(
                    self.closed_reason(reason.unwrap_or_else(|| "the server closed the connection".into())),
                );
            }
            _ => return Flow::Continue,
        };
        let msg = match serde_json::from_str::<ServerMsg>(&body) {
            Ok(msg) => msg,
            Err(err) => {
                debug!("ignoring a server message: {err}");
                return Flow::Continue;
            }
        };
        let session_id = match &msg {
            ServerMsg::Welcome(_) => return Flow::Continue,
            ServerMsg::Ready(m) => m.session_id,
            ServerMsg::Progress(m) => m.session_id,
            ServerMsg::Final(m) => m.session_id,
            ServerMsg::Cancelled(m) => m.session_id,
            ServerMsg::Error(err) => match err.session_id {
                Some(id) => id,
                None => {
                    warn!("server error: {}", err.message);
                    self.connection_error = Some(err.clone());
                    return Flow::Continue;
                }
            },
        };
        let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) else {
            debug!("ignoring a message for session {session_id}, which isn't active");
            return Flow::Continue;
        };
        active.last_heard = at;
        let event = match msg {
            ServerMsg::Ready(ready) => {
                active.got_ready = true;
                active.streaming = ready.need_audio;
                if !ready.need_audio {
                    active.frames = None;
                }
                if let Some(tx) = &active.ready {
                    tx.send_replace(Some(ready.need_audio));
                }
                SessionEvent::Ready {
                    resumed: ready.resumed,
                    need_audio: ready.need_audio,
                    model_state: ready.model_state,
                }
            }
            ServerMsg::Progress(p) => SessionEvent::Progress { state: p.state, received_ms: p.received_ms },
            ServerMsg::Final(f) => SessionEvent::Final(f),
            ServerMsg::Cancelled(_) => SessionEvent::Cancelled,
            ServerMsg::Error(err) if active.got_ready => SessionEvent::Error(err),
            ServerMsg::Error(err) => SessionEvent::Rejected(err),
            ServerMsg::Welcome(_) => unreachable!("handled above"),
        };
        let terminal = event.is_terminal();
        if !active.ended {
            let _ = active.events.send(event);
        }
        if terminal {
            self.active = None;
            if let Some(next) = self.queued.pop_front() {
                return self.send_start(next).await;
            }
        }
        Flow::Continue
    }

    async fn on_outgoing(&mut self, out: Option<Outgoing>) -> Flow {
        let Some(out) = out else {
            // Every sender is gone without `stop`: nothing more will come.
            if let Some(active) = &mut self.active {
                active.frames = None;
            }
            return Flow::Continue;
        };
        let id = self.active.as_ref().map(|a| a.id).expect("outgoing audio needs an active session");
        let mut out = Some(out);
        let mut written = 0;
        while let Some(next) = out.take() {
            match next {
                Outgoing::Frame(frame) => {
                    if let Err(err) = self.sink.feed(Message::binary(frame_bytes(&frame))).await {
                        return Flow::Close(format!("cannot write: {err}"));
                    }
                    written += 1;
                    if written < WRITE_BATCH {
                        out = self.active.as_mut().and_then(|a| a.frames.as_mut()).and_then(|rx| rx.try_recv().ok());
                    }
                }
                Outgoing::Stop(samples) => {
                    if let Some(active) = &mut self.active {
                        active.frames = None;
                    }
                    if let Err(err) = self.sink.feed(text(&ClientMsg::Stop(StopMsg { session_id: id, samples }))).await
                    {
                        return Flow::Close(format!("cannot write: {err}"));
                    }
                }
            }
        }
        match self.sink.flush().await {
            Ok(()) => Flow::Continue,
            Err(err) => Flow::Close(format!("cannot write: {err}")),
        }
    }
}

/// The active session's next piece of audio, once it may be sent. Pending otherwise.
async fn next_outgoing(active: &mut Option<Active>) -> Option<Outgoing> {
    match active {
        Some(Active { streaming: true, frames: Some(rx), .. }) => rx.recv().await,
        _ => std::future::pending().await,
    }
}

fn frame_bytes(frame: &AudioFrame) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(FRAME_BYTES);
    for sample in frame {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
