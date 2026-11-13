//! The tasks that own one WebSocket connection. The actor routes what the server sends to the
//! active session, decides what to send, and watches for `progress` silence; a reader task and a
//! writer task do the socket I/O, so a stalled socket never stops the watchdog, `cancel` or
//! `close`.

use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use psst_proto::{
    Endpoint, PROTOCOL_VERSION, SessionId, Token,
    ws::{AudioFormat, CancelMsg, ClientInfo, ClientMsg, ErrorMsg, HelloMsg, ReportMsg, ServerMsg, StartMsg, StopMsg},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpStream, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant, sleep_until},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, client::IntoClientRequest, http::HeaderValue},
};
use tracing::{debug, warn};

use crate::{AudioFrame, Client, ClientError, LostReason, PROGRESS_TIMEOUT, SessionEvent, StartOpts, codec::Encoder};

/// Messages written before one flush.
const WRITE_BATCH: usize = 50;
/// Audio frames handed to the writer and not yet written: 1 s. Further frames wait in the
/// session's buffer, so uploads keep their backpressure.
const IN_FLIGHT: usize = 50;
/// How long queued messages may take to drain once the connection is no longer needed.
const DRAIN_GRACE: Duration = Duration::from_secs(30);
/// How long the writer may take to finish after the connection was lost or closed.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Round trips [`Client::round_trip`] looks back over.
const ROUND_TRIPS: usize = 4;

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
    pub encoder: Encoder,
}

pub(crate) enum Outgoing {
    Frame(Box<AudioFrame>),
    Stop(u64),
}

/// State the handles read without asking the task.
pub(crate) struct Shared {
    last_received: Mutex<Instant>,
    /// The last [`ROUND_TRIPS`] round trips measured, oldest first.
    round_trips: Mutex<VecDeque<Duration>>,
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

    pub fn round_trip(&self) -> Duration {
        self.round_trips.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().max().unwrap_or_default()
    }

    fn measured(&self, round_trip: Duration) {
        let mut round_trips = self.round_trips.lock().unwrap_or_else(|e| e.into_inner());
        if round_trips.len() == ROUND_TRIPS {
            round_trips.pop_front();
        }
        round_trips.push_back(round_trip);
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
    let hello_sent = Instant::now();
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
    let shared = Arc::new(Shared {
        last_received: Mutex::new(Instant::now()),
        round_trips: Mutex::new(VecDeque::from([hello_sent.elapsed()])),
        closed,
    });
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (sink, mut stream) = ws.split();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let (writer_tx, writer_rx) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(write_loop(sink, writer_rx, incoming_tx.clone()));
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
        writer: writer_tx,
        writer_task,
        permits: Arc::new(Semaphore::new(IN_FLIGHT)),
        incoming: incoming_rx,
        cmds: cmd_rx,
        cmds_open: true,
        active: None,
        queued: VecDeque::new(),
        pings: Vec::new(),
        ping_sent: None,
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

/// What the writer task writes, in order.
enum Out {
    Msg(Message),
    /// An encoded audio frame and its place among the frames in flight, freed once it's written.
    Frame(Vec<u8>, OwnedSemaphorePermit),
}

/// Writes messages in order, flushing after each batch. Ends when the actor drops its sender
/// (then it closes the WebSocket) or a write fails (then it tells the actor).
async fn write_loop(
    mut sink: SplitSink<Ws, Message>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    incoming: mpsc::UnboundedSender<Incoming>,
) {
    let failed = |err: tungstenite::Error| {
        let _ = incoming.send(Incoming::Closed(format!("cannot write: {err}")));
    };
    while let Some(first) = rx.recv().await {
        let mut next = Some(first);
        let mut batch = 0;
        while let Some(out) = next.take() {
            let result = match out {
                Out::Msg(msg) => sink.feed(msg).await,
                Out::Frame(frame, _permit) => sink.feed(Message::binary(frame)).await,
            };
            if let Err(err) = result {
                return failed(err);
            }
            batch += 1;
            if batch < WRITE_BATCH {
                next = rx.try_recv().ok();
            }
        }
        if let Err(err) = sink.flush().await {
            return failed(err);
        }
    }
    let _ = sink.send(Message::Close(None)).await;
    let _ = sink.close().await;
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
    encoder: Encoder,
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
    writer: mpsc::UnboundedSender<Out>,
    writer_task: JoinHandle<()>,
    permits: Arc<Semaphore>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    cmds_open: bool,
    active: Option<Active>,
    queued: VecDeque<NewSession>,
    pings: Vec<oneshot::Sender<()>>,
    /// When the ping that's still unanswered went out.
    ping_sent: Option<Instant>,
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
        let (reason, grace) = loop {
            if !self.cmds_open && !self.has_pending_audio() && self.queued.is_empty() {
                break ("no handle uses the connection any more".to_string(), DRAIN_GRACE);
            }
            let deadline = self.active.as_ref().map(|a| a.last_heard + PROGRESS_TIMEOUT);
            let flow = tokio::select! {
                biased;
                incoming = self.incoming.recv() => match incoming {
                    Some(Incoming::Msg(msg, at)) => self.on_message(msg, at),
                    Some(Incoming::Closed(reason)) => Flow::Close(self.closed_reason(reason)),
                    None => Flow::Close("the reader stopped".into()),
                },
                cmd = self.cmds.recv(), if self.cmds_open => match cmd {
                    Some(cmd) => self.on_cmd(cmd),
                    None => {
                        self.cmds_open = false;
                        Flow::Continue
                    }
                },
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.lose_all(LostReason::ProgressTimeout);
                    Flow::Close(LostReason::ProgressTimeout.to_string())
                }
                (permit, out) = next_outgoing(&mut self.active, &self.permits) => self.on_outgoing(permit, out),
            };
            if let Flow::Close(reason) = flow {
                break (reason, CLOSE_GRACE);
            }
        };
        debug!("closing the connection: {reason}");
        self.reader.abort();
        self.lose_all(LostReason::ConnectionClosed { reason: reason.clone() });
        self.shared.closed.send_replace(Some(reason));
        // The writer drains what's queued and closes the WebSocket, within `grace`. Pending pings
        // fail when their senders drop with `self`.
        let Actor { writer, mut writer_task, .. } = self;
        drop(writer);
        tokio::spawn(async move {
            if tokio::time::timeout(grace, &mut writer_task).await.is_err() {
                writer_task.abort();
            }
        });
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

    /// Queues a message for the writer. A failed write comes back as `Incoming::Closed`.
    fn write(&mut self, msg: Message) -> Flow {
        match self.writer.send(Out::Msg(msg)) {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Close("the writer stopped".into()),
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) -> Flow {
        match cmd {
            Cmd::Start(new) => {
                if self.active.is_some() {
                    self.queued.push_back(*new);
                    Flow::Continue
                } else {
                    self.send_start(*new)
                }
            }
            Cmd::Cancel(id) => self.cancel(id, None),
            Cmd::Fail(id, reason) => self.cancel(id, Some(reason)),
            Cmd::Report(report) => self.write(text(&ClientMsg::Report(report))),
            Cmd::Ping(tx) => {
                let first = self.pings.is_empty();
                self.pings.push(tx);
                if !first {
                    return Flow::Continue;
                }
                self.ping_sent = Some(Instant::now());
                self.write(Message::Ping(Default::default()))
            }
            Cmd::Close => Flow::Close("closed by the client".into()),
        }
    }

    fn send_start(&mut self, new: NewSession) -> Flow {
        let NewSession { opts, events, frames, ready, encoder } = new;
        let start = ClientMsg::Start(StartMsg {
            session_id: opts.session_id,
            profile: opts.profile,
            audio: AudioFormat { codec: encoder.codec(), ..AudioFormat::default() },
            mode: opts.mode,
            resume: opts.resume,
            eval: opts.eval,
        });
        self.active = Some(Active {
            id: opts.session_id,
            events,
            frames: Some(frames),
            encoder,
            streaming: false,
            ready: Some(ready),
            got_ready: false,
            ended: false,
            last_heard: Instant::now(),
        });
        self.write(text(&start))
    }

    /// Cancels a session: on the server if its `start` went out, locally if it's still queued.
    /// With `failure`, the owner sees that error instead of `Cancelled`.
    fn cancel(&mut self, id: SessionId, failure: Option<String>) -> Flow {
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
        self.write(text(&ClientMsg::Cancel(CancelMsg { session_id: id })))
    }

    fn on_message(&mut self, msg: Message, at: Instant) -> Flow {
        // Measured before the pings are answered, so their callers see it.
        if matches!(msg, Message::Pong(_))
            && let Some(sent) = self.ping_sent.take()
        {
            self.shared.measured(at.saturating_duration_since(sent));
        }
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
                return self.send_start(next);
            }
        }
        Flow::Continue
    }

    fn on_outgoing(&mut self, permit: OwnedSemaphorePermit, out: Option<Outgoing>) -> Flow {
        let Some(active) = &mut self.active else { return Flow::Continue };
        let out = match out {
            Some(Outgoing::Frame(frame)) => match active.encoder.encode(&frame) {
                Ok(bytes) => Out::Frame(bytes, permit),
                Err(message) => {
                    let id = active.id;
                    return self.cancel(id, Some(message));
                }
            },
            Some(Outgoing::Stop(samples)) => {
                active.frames = None;
                Out::Msg(text(&ClientMsg::Stop(StopMsg { session_id: active.id, samples })))
            }
            None => {
                // Every sender is gone without `stop`: nothing more will come.
                active.frames = None;
                return Flow::Continue;
            }
        };
        match self.writer.send(out) {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Close("the writer stopped".into()),
        }
    }
}

/// The active session's next piece of audio, once it may be sent and the writer has room for
/// it. Pending otherwise. Cancel-safe: a permit taken without a frame is simply released.
async fn next_outgoing(
    active: &mut Option<Active>,
    permits: &Arc<Semaphore>,
) -> (OwnedSemaphorePermit, Option<Outgoing>) {
    match active {
        Some(Active { streaming: true, frames: Some(rx), .. }) => {
            let permit = permits.clone().acquire_owned().await.expect("the semaphore is never closed");
            (permit, rx.recv().await)
        }
        _ => std::future::pending().await,
    }
}
