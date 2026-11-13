//! A scripted stand-in for `psst-server` that speaks `/v1/client` (and `/api/pair/redeem`), for
//! client and daemon tests that need what the real server doesn't do on demand: stop reading
//! mid-session, hold `ready` back, refuse connections while keeping its registry, or reject the
//! next `start`. End-to-end tests use the real server (`spawn_server`).
//!
//! It keeps a session registry like the real one: `start` gets `ready` at once, `progress`
//! follows every [`FakeServerConfig::progress_interval`], `stop` is checked against the audio
//! received and answered after [`FakeServerConfig::final_delay`] with the next scripted
//! [`Outcome`]. Finished sessions answer `resume` with their `final`; running ones are attached.
//! A disconnect drops sessions still recording and keeps stopped ones running. A device that
//! connects again replaces its older connection, as with the real server. Its `welcome` offers
//! only `pcm_s16le`, as servers from before Opus do, so [`SeenSession::audio`] is exactly what the
//! client sent.
//!
//! It runs on its own thread with its own runtime, so sync and async tests use it the same way.

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use psst_proto::{
    Endpoint, ErrorCode, FRAME_SAMPLES, Final, HistoryId, ServerTimings, SessionId,
    api::{ApiError, ApiErrorCode, PairRedeem, PairRedeemed},
    ws::{
        CancelledMsg, ClientMsg, Codec, ErrorMsg, FinalMsg, ProgressMsg, ProgressState, ReadyModelState, ReadyMsg,
        ReportMsg, ServerLimits, ServerMsg, StartMsg, WelcomeMsg,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UnixListener},
    sync::{Notify, broadcast},
};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
};

/// How a stopped session ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `final` with this text (`""` is "nothing heard").
    Final(String),
    /// `error` with this code.
    Error(ErrorCode),
    /// Never finishes; `progress` keeps coming.
    Hang,
}

#[derive(Debug, Clone)]
pub struct FakeServerConfig {
    /// Also listen on a free TCP port on 127.0.0.1.
    pub tcp: bool,
    /// The device token TCP clients must present.
    pub token: String,
    /// The code `/api/pair/redeem` accepts; it hands out [`FakeServerConfig::token`].
    pub pair_code: String,
    pub max_recording_s: u32,
    pub progress_interval: Duration,
    /// From `stop` (or a resume's `ready`) until the outcome is sent.
    pub final_delay: Duration,
    /// `ready.model_state`.
    pub model_state: ReadyModelState,
    /// Holds each `ready` back this long, like a slow link.
    pub ready_delay: Duration,
}

impl Default for FakeServerConfig {
    fn default() -> Self {
        Self {
            tcp: false,
            token: "fake-token".into(),
            pair_code: "PAIRCODE".into(),
            max_recording_s: 600,
            progress_interval: Duration::from_millis(500),
            final_delay: Duration::from_millis(50),
            model_state: ReadyModelState::Loaded,
            ready_delay: Duration::ZERO,
        }
    }
}

/// What the fake server saw of one session.
#[derive(Debug, Clone, PartialEq)]
pub struct SeenSession {
    pub id: SessionId,
    /// Every `start` for this id, in order (a resume adds one).
    pub starts: Vec<StartMsg>,
    /// The audio of the latest upload or live stream, padding included.
    pub audio: Vec<i16>,
    pub stop_samples: Option<u64>,
    pub cancelled: bool,
    /// The `final` text sent, if any.
    pub final_text: Option<String>,
}

#[derive(Debug, Clone)]
enum RegState {
    Recording,
    Running { done_at: Instant, outcome: Outcome },
    Finished(Box<FinalMsg>),
}

#[derive(Debug)]
struct Reg {
    state: RegState,
    history_id: HistoryId,
    /// The connection the session is attached to.
    conn: Option<u64>,
}

#[derive(Debug, Default)]
struct State {
    outcomes: VecDeque<Outcome>,
    rejections: VecDeque<ErrorCode>,
    stalled: bool,
    registry: HashMap<SessionId, Reg>,
    seen: Vec<SeenSession>,
    reports: Vec<ReportMsg>,
    connections: u64,
    next_history: HistoryId,
}

impl State {
    fn seen(&mut self, id: SessionId) -> &mut SeenSession {
        if let Some(pos) = self.seen.iter().position(|s| s.id == id) {
            return &mut self.seen[pos];
        }
        self.seen.push(SeenSession {
            id,
            starts: Vec::new(),
            audio: Vec::new(),
            stop_samples: None,
            cancelled: false,
            final_text: None,
        });
        self.seen.last_mut().expect("just pushed")
    }
}

struct Inner {
    cfg: FakeServerConfig,
    state: Mutex<State>,
    disconnect: broadcast::Sender<()>,
    /// (device, connection) of each new connection, so the device's older ones close.
    replaced: broadcast::Sender<(String, u64)>,
    listening: tokio::sync::watch::Sender<bool>,
    shutdown: Notify,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A running fake server. Stops on drop.
pub struct FakeServer {
    inner: Arc<Inner>,
    socket: PathBuf,
    tcp: Option<SocketAddr>,
    thread: Option<thread::JoinHandle<()>>,
    _dir: Option<tempfile::TempDir>,
}

impl FakeServer {
    /// Starts on a socket in a new temp dir.
    pub fn start(cfg: FakeServerConfig) -> FakeServer {
        let dir = tempfile::Builder::new().prefix("psst-fake-server-").tempdir().expect("temp dir");
        let mut server = Self::start_at(&dir.path().join("client.sock"), cfg);
        server._dir = Some(dir);
        server
    }

    /// Starts on `socket`, which must not exist yet.
    pub fn start_at(socket: &Path, cfg: FakeServerConfig) -> FakeServer {
        let tcp = cfg.tcp.then(|| std::net::TcpListener::bind("127.0.0.1:0").expect("bind tcp"));
        let tcp_addr = tcp.as_ref().map(|l| l.local_addr().expect("tcp addr"));
        let unix = std::os::unix::net::UnixListener::bind(socket).expect("bind fake server socket");
        let (disconnect, _) = broadcast::channel(4);
        let (replaced, _) = broadcast::channel(16);
        let (listening, _) = tokio::sync::watch::channel(true);
        let inner =
            Arc::new(Inner { cfg, state: Mutex::default(), disconnect, replaced, listening, shutdown: Notify::new() });
        let thread_inner = inner.clone();
        let thread = thread::Builder::new()
            .name("fake-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
                rt.block_on(serve(thread_inner, unix, tcp));
            })
            .expect("spawn fake server");
        FakeServer { inner, socket: socket.into(), tcp: tcp_addr, thread: Some(thread), _dir: None }
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    pub fn tcp_endpoint(&self) -> Option<Endpoint> {
        self.tcp.map(|addr| Endpoint::Tcp { host: addr.ip().to_string(), port: addr.port() })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Queues the outcome of the next stopped session.
    pub fn push_outcome(&self, outcome: Outcome) {
        self.inner.state().outcomes.push_back(outcome);
    }

    /// Rejects the next `start` with this code, before `ready`.
    pub fn reject_next(&self, code: ErrorCode) {
        self.inner.state().rejections.push_back(code);
    }

    /// While stalled, the server neither reads nor writes on any connection: no `progress`, no
    /// pong, no `final`. Connections stay open.
    pub fn set_stalled(&self, stalled: bool) {
        self.inner.state().stalled = stalled;
    }

    /// Closes every connection.
    pub fn disconnect_all(&self) {
        let _ = self.inner.disconnect.send(());
    }

    /// Stops or resumes accepting connections. Stopping also closes every connection; the
    /// registry is kept.
    pub fn set_listening(&self, listening: bool) {
        self.inner.listening.send_replace(listening);
        if !listening {
            self.disconnect_all();
        }
    }

    /// Every session seen so far, in the order of their first `start`.
    pub fn sessions(&self) -> Vec<SeenSession> {
        self.inner.state().seen.clone()
    }

    pub fn session(&self, id: SessionId) -> Option<SeenSession> {
        self.inner.state().seen.iter().find(|s| s.id == id).cloned()
    }

    pub fn reports(&self) -> Vec<ReportMsg> {
        self.inner.state().reports.clone()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.inner.state().connections
    }

    /// Polls `check` until it's true or `timeout` passes; returns whether it became true.
    pub fn wait_until(&self, timeout: Duration, mut check: impl FnMut(&FakeServer) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if check(self) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.inner.shutdown.notify_one();
        self.disconnect_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn serve(inner: Arc<Inner>, unix: std::os::unix::net::UnixListener, tcp: Option<std::net::TcpListener>) {
    let socket = unix.local_addr().ok().and_then(|a| a.as_pathname().map(Path::to_path_buf)).expect("socket path");
    let tcp_addr = tcp.as_ref().map(|l| l.local_addr().expect("tcp addr"));
    let mut unix = Some(unix);
    let mut tcp = tcp;
    let mut listeners: Option<(UnixListener, Option<TcpListener>)> = None;
    let mut listening = inner.listening.subscribe();
    loop {
        if *listening.borrow_and_update() {
            if listeners.is_none() {
                // Rebind after a pause: connecting fails while not listening, as with a stopped
                // server.
                let u = unix.take().unwrap_or_else(|| {
                    let _ = std::fs::remove_file(&socket);
                    std::os::unix::net::UnixListener::bind(&socket).expect("rebind socket")
                });
                u.set_nonblocking(true).expect("nonblocking");
                let t = tcp.take().or_else(|| tcp_addr.map(|a| std::net::TcpListener::bind(a).expect("rebind tcp")));
                let t = t.map(|l| {
                    l.set_nonblocking(true).expect("nonblocking");
                    TcpListener::from_std(l).expect("tcp listener")
                });
                listeners = Some((UnixListener::from_std(u).expect("unix listener"), t));
            }
        } else {
            listeners = None;
        }
        let accept_unix = async {
            match &listeners {
                Some((u, _)) => u.accept().await.ok().map(|(s, _)| s),
                None => std::future::pending().await,
            }
        };
        let accept_tcp = async {
            match &listeners {
                Some((_, Some(t))) => t.accept().await.ok().map(|(s, _)| s),
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = inner.shutdown.notified() => return,
            _ = listening.changed() => {}
            stream = accept_unix => if let Some(stream) = stream {
                tokio::spawn(connection(inner.clone(), stream, false));
            },
            stream = accept_tcp => if let Some(stream) = stream {
                let _ = stream.set_nodelay(true);
                tokio::spawn(connection(inner.clone(), stream, true));
            },
        }
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Unpin>(inner: Arc<Inner>, mut stream: S, tcp: bool) {
    // Subscribe before the handshake, then check `listening`: `set_listening(false)` turns it
    // off before it disconnects, so a connection still in its handshake sees one or the other.
    let mut disconnect = inner.disconnect.subscribe();
    if !*inner.listening.borrow() {
        return;
    }
    // `/api/pair/redeem` is plain HTTP; everything else is the WebSocket.
    const PAIR: &[u8] = b"POST /api/pair/redeem";
    let mut head = [0u8; PAIR.len()];
    let mut buffered = Vec::new();
    // Peek by reading the request line's start; replay it to the WebSocket handshake.
    while buffered.len() < PAIR.len() {
        let want = PAIR.len() - buffered.len();
        match stream.read(&mut head[..want]).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffered.extend_from_slice(&head[..n]),
        }
    }
    if buffered.starts_with(PAIR) {
        pair(&inner, Prefixed { prefix: buffered, pos: 0, inner: stream }).await;
        return;
    }
    let stream = Prefixed { prefix: buffered, pos: 0, inner: stream };
    let mut auth = None;
    #[allow(clippy::result_large_err)] // the error type is tungstenite's
    let callback = |req: &Request, resp: Response| {
        auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
        Ok(resp)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { return };
    let conn_id = {
        let mut state = inner.state();
        state.connections += 1;
        state.connections
    };
    let send = |msg: &ServerMsg| Message::text(serde_json::to_string(msg).expect("serialise"));
    if tcp && auth.as_deref() != Some(&format!("Bearer {}", inner.cfg.token)) {
        let err = ErrorMsg::new(None, ErrorCode::Unauthorized, "unknown device token");
        let _ = ws.send(send(&ServerMsg::Error(err))).await;
        let _ = ws.close(None).await;
        return;
    }
    // hello → welcome
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(body))) => match serde_json::from_str::<ClientMsg>(&body) {
                Ok(ClientMsg::Hello(_)) => break,
                _ => return,
            },
            Some(Ok(_)) => {}
            _ => return,
        }
    }
    let device: String = if tcp { "fake-device".into() } else { "local:test".into() };
    let welcome = WelcomeMsg {
        server_version: "fake".into(),
        device: device.clone(),
        limits: ServerLimits { max_recording_s: inner.cfg.max_recording_s },
        codecs: vec![Codec::PcmS16le],
    };
    if ws.send(send(&ServerMsg::Welcome(welcome))).await.is_err() {
        return;
    }
    let mut replaced = inner.replaced.subscribe();
    let _ = inner.replaced.send((device.clone(), conn_id));
    let replaced_by_newer = |msg: Result<(String, u64), _>| matches!(msg, Ok((d, id)) if d == device && id > conn_id);

    let mut active: Option<(SessionId, bool)> = None; // (id, need_audio)
    let mut next_progress = tokio::time::Instant::now();
    loop {
        if inner.state().stalled {
            tokio::select! {
                _ = disconnect.recv() => break,
                msg = replaced.recv() => if replaced_by_newer(msg) { break } else { continue },
                _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
            }
        }
        // A session another connection took over (a resume) is no longer this one's.
        if let Some((id, _)) = active
            && !attached(&inner.state(), id, conn_id)
        {
            active = None;
        }
        let done_at = active.and_then(|(id, _)| match inner.state().registry.get(&id).map(|r| &r.state) {
            Some(RegState::Running { done_at, outcome }) if *outcome != Outcome::Hang => Some(*done_at),
            _ => None,
        });
        let mut out = Vec::new();
        tokio::select! {
            // In order, so a disconnect wins over a message that arrived with it: under load this
            // task may first run after both, and after `set_listening(false)` no `start` may get in.
            biased;
            _ = disconnect.recv() => break,
            msg = replaced.recv() => if replaced_by_newer(msg) { break },
            _ = tokio::time::sleep(Duration::from_millis(10)) => {} // re-check `stalled`
            _ = tokio::time::sleep_until(next_progress), if active.is_some() => {
                next_progress += inner.cfg.progress_interval;
                let (id, _) = active.expect("checked");
                let state = inner.state();
                let (pstate, received) = match state.registry.get(&id) {
                    Some(reg) => (match reg.state {
                        RegState::Recording => ProgressState::Recording,
                        _ => ProgressState::Transcribing,
                    }, state.seen.iter().find(|s| s.id == id).map_or(0, |s| s.audio.len() as u64 / 16)),
                    None => (ProgressState::Recording, 0),
                };
                out.push(ServerMsg::Progress(ProgressMsg { session_id: id, state: pstate, received_ms: received }));
            }
            _ = tokio::time::sleep_until(done_at.map(Into::into).unwrap_or_else(tokio::time::Instant::now)), if done_at.is_some() => {
                let (id, _) = active.expect("checked");
                out.extend(finish(&inner, id));
                active = None;
            }
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    Message::Binary(bytes) => {
                        if let Some((id, true)) = active {
                            let mut state = inner.state();
                            if attached(&state, id, conn_id)
                                && matches!(state.registry.get(&id).map(|r| &r.state), Some(RegState::Recording))
                            {
                                let samples = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b));
                                state.seen(id).audio.extend(samples);
                            }
                        }
                    }
                    Message::Text(body) => {
                        let Ok(msg) = serde_json::from_str::<ClientMsg>(&body) else {
                            out.push(ServerMsg::Error(ErrorMsg::new(None, ErrorCode::BadRequest, "bad message")));
                            continue;
                        };
                        out.extend(on_client_msg(&inner, conn_id, &mut active, msg, &mut next_progress));
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
        for msg in out {
            if matches!(msg, ServerMsg::Ready(_)) && !inner.cfg.ready_delay.is_zero() {
                tokio::time::sleep(inner.cfg.ready_delay).await;
            }
            if ws.send(send(&msg)).await.is_err() {
                break;
            }
        }
    }
    // Disconnected: a session still recording is dropped; a stopped one keeps running.
    if let Some((id, _)) = active
        && attached(&inner.state(), id, conn_id)
    {
        let mut state = inner.state();
        match state.registry.get_mut(&id) {
            Some(reg) if matches!(reg.state, RegState::Recording) => {
                state.registry.remove(&id);
            }
            Some(reg) => reg.conn = None,
            None => {}
        }
    }
    let _ = ws.close(None).await;
}

fn attached(state: &State, id: SessionId, conn_id: u64) -> bool {
    state.registry.get(&id).is_some_and(|reg| reg.conn == Some(conn_id))
}

fn on_client_msg(
    inner: &Inner,
    conn_id: u64,
    active: &mut Option<(SessionId, bool)>,
    msg: ClientMsg,
    next_progress: &mut tokio::time::Instant,
) -> Vec<ServerMsg> {
    let mut state = inner.state();
    match msg {
        ClientMsg::Start(start) => {
            let id = start.session_id;
            state.seen(id).starts.push(start.clone());
            if active.is_some() {
                return vec![ServerMsg::Error(ErrorMsg::new(Some(id), ErrorCode::BadRequest, "a session is active"))];
            }
            if let Some(code) = state.rejections.pop_front() {
                return vec![ServerMsg::Error(ErrorMsg::new(Some(id), code, format!("rejected: {code:?}")))];
            }
            let existing = state.registry.get(&id).map(|r| r.state.clone());
            if !start.resume && existing.is_some() {
                return vec![ServerMsg::Error(ErrorMsg::new(Some(id), ErrorCode::BadRequest, "session id reused"))];
            }
            *next_progress = tokio::time::Instant::now() + inner.cfg.progress_interval;
            let ready = |resumed, need_audio| {
                ServerMsg::Ready(ReadyMsg { session_id: id, model_state: inner.cfg.model_state, resumed, need_audio })
            };
            match existing {
                Some(RegState::Finished(fin)) if start.resume => {
                    vec![ready(true, false), ServerMsg::Final(fin)]
                }
                Some(RegState::Running { .. }) if start.resume => {
                    state.registry.get_mut(&id).expect("exists").conn = Some(conn_id);
                    *active = Some((id, false));
                    vec![ready(true, false)]
                }
                _ => {
                    let history_id = state.next_history + 1;
                    state.next_history = history_id;
                    state.registry.insert(id, Reg { state: RegState::Recording, history_id, conn: Some(conn_id) });
                    state.seen(id).audio.clear();
                    *active = Some((id, true));
                    vec![ready(false, true)]
                }
            }
        }
        ClientMsg::Stop(stop) => {
            let id = stop.session_id;
            if active.map(|(a, _)| a) != Some(id) {
                return vec![];
            }
            let received = state.seen(id).audio.len() as u64;
            state.seen(id).stop_samples = Some(stop.samples);
            if !(stop.samples <= received && received < stop.samples + FRAME_SAMPLES as u64) {
                state.registry.remove(&id);
                *active = None;
                let msg = format!("stop says {} samples, received {received}", stop.samples);
                return vec![ServerMsg::Error(ErrorMsg::new(Some(id), ErrorCode::BadAudio, msg))];
            }
            let outcome =
                state.outcomes.pop_front().unwrap_or_else(|| Outcome::Final(format!("heard {} ms", stop.samples / 16)));
            let done_at = Instant::now() + inner.cfg.final_delay;
            if let Some(reg) = state.registry.get_mut(&id) {
                reg.state = RegState::Running { done_at, outcome };
            }
            vec![]
        }
        ClientMsg::Cancel(cancel) => {
            let id = cancel.session_id;
            if active.map(|(a, _)| a) != Some(id) {
                return vec![];
            }
            state.registry.remove(&id);
            state.seen(id).cancelled = true;
            *active = None;
            vec![ServerMsg::Cancelled(CancelledMsg { session_id: id })]
        }
        ClientMsg::Report(report) => {
            state.reports.push(report);
            vec![]
        }
        ClientMsg::Hello(_) => vec![],
    }
}

/// The outcome of a running session whose time has come.
fn finish(inner: &Inner, id: SessionId) -> Option<ServerMsg> {
    let mut state = inner.state();
    let reg = state.registry.get_mut(&id)?;
    let RegState::Running { outcome, .. } = reg.state.clone() else { return None };
    match outcome {
        Outcome::Final(text) => {
            let fin = Box::new(FinalMsg {
                session_id: id,
                history_id: reg.history_id,
                r#final: Final {
                    text: text.clone(),
                    raw_text: text.trim().into(),
                    edits: vec![],
                    language: "en".into(),
                },
                timings: ServerTimings { attempts: 1, ..ServerTimings::default() },
                trace: None,
            });
            reg.state = RegState::Finished(fin.clone());
            reg.conn = None;
            state.seen(id).final_text = Some(text);
            Some(ServerMsg::Final(fin))
        }
        Outcome::Error(code) => {
            state.registry.remove(&id);
            Some(ServerMsg::Error(ErrorMsg::new(Some(id), code, format!("failed: {code:?}"))))
        }
        Outcome::Hang => None,
    }
}

async fn pair<S: AsyncRead + AsyncWrite + Unpin>(inner: &Inner, mut stream: S) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let body = loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buf.len() >= end + 4 + len {
            break buf[end + 4..end + 4 + len].to_vec();
        }
    };
    let (status, json) = match serde_json::from_slice::<PairRedeem>(&body) {
        Ok(req) if req.code == inner.cfg.pair_code => (
            "200 OK",
            serde_json::to_string(&PairRedeemed {
                device_id: "fake-device".into(),
                token: psst_proto::Token::new(inner.cfg.token.clone()),
            }),
        ),
        Ok(_) => (
            "403 Forbidden",
            serde_json::to_string(&ApiError { code: ApiErrorCode::Forbidden, message: "wrong code".into() }),
        ),
        Err(_) => (
            "400 Bad Request",
            serde_json::to_string(&ApiError { code: ApiErrorCode::BadRequest, message: "bad body".into() }),
        ),
    };
    let json = json.expect("serialise");
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
        json.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A stream with some already-read bytes put back in front.
struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
