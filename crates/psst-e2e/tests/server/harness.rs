//! What the server tests share: a server with the fake engine, a raw client of the `/v1/client`
//! protocol, the admin CLI, and plain HTTP over TCP.

use std::{
    net::SocketAddr,
    path::Path,
    process::{Command, Output},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use psst_engine_proto::fake::{LoggedRequest, Script, TranscribeStep};
use psst_proto::{
    FRAME_SAMPLES, SessionId,
    api::{ModelState, ModelStatus},
    ws::{
        AudioFormat, CancelMsg, ClientInfo, ClientMsg, ErrorMsg, EvalOpts, FinalMsg, HelloMsg, Mode, ProgressMsg,
        ReadyMsg, ServerMsg, StartMsg, StopMsg, WelcomeMsg,
    },
};
use psst_testkit::{TestServer, TestServerConfig};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UnixStream},
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Message, client::IntoClientRequest, http::header::AUTHORIZATION},
};

pub const SERVER: &str = env!("CARGO_BIN_EXE_psst-server-e2e");

/// How long any single expected message may take.
pub const MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// One second of audio.
pub const SECOND: usize = 16_000;

/// A server with the fake engine running `script`, with `cfg` adjusted by the caller.
pub fn server(script: Script, adjust: impl FnOnce(&mut TestServerConfig)) -> TestServer {
    let mut cfg = TestServerConfig { script, ..TestServerConfig::default() };
    adjust(&mut cfg);
    psst_testkit::spawn_server(Path::new(SERVER), cfg)
}

/// A server whose engine answers every request with the default result.
pub fn default_server() -> TestServer {
    server(Script::default(), |_| {})
}

pub fn result(text: &str, delay_ms: u64) -> TranscribeStep {
    TranscribeStep::Result { text: text.into(), delay_ms, no_speech_prob: None, speech_ms: None }
}

pub fn script(steps: Vec<TranscribeStep>) -> Script {
    Script { transcribe: steps, ..Script::default() }
}

/// Runs `psst-server <args> --socket <server's socket>`.
pub fn admin(server: &TestServer, args: &[&str]) -> Output {
    Command::new(SERVER).args(args).arg("--socket").arg(server.socket()).output().expect("run psst-server")
}

/// Runs an admin command with `--json` and parses its output; panics if it fails.
pub fn admin_json(server: &TestServer, args: &[&str]) -> Value {
    let out = Command::new(SERVER)
        .args(args)
        .args(["--json", "--socket"])
        .arg(server.socket())
        .output()
        .expect("run psst-server");
    assert!(out.status.success(), "psst-server {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("JSON output")
}

pub fn model_status(server: &TestServer) -> ModelStatus {
    serde_json::from_value(admin_json(server, &["model", "status"])).expect("a ModelStatus")
}

/// Waits until the model is in `state`.
pub async fn wait_model(server: &TestServer, state: ModelState) -> ModelStatus {
    let deadline = tokio::time::Instant::now() + MESSAGE_TIMEOUT;
    loop {
        let status = model_status(server);
        if status.state == state {
            return status;
        }
        assert!(tokio::time::Instant::now() < deadline, "model never became {state:?}; last {status:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The `transcribe` requests the engine received.
pub fn transcribes(server: &TestServer) -> Vec<LoggedRequest> {
    requests_of(server, "transcribe")
}

pub fn requests_of(server: &TestServer, kind: &str) -> Vec<LoggedRequest> {
    server.engine_requests().into_iter().filter(|r| r.header["type"] == kind).collect()
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A WebSocket on `/v1/client` that hasn't said `hello` yet.
pub struct Socket {
    ws: WebSocketStream<Box<dyn Io>>,
}

impl Socket {
    pub async fn unix(path: &Path) -> Socket {
        let stream = UnixStream::connect(path).await.expect("connect to the socket");
        Self::open(Box::new(stream), None).await
    }

    pub async fn tcp(addr: SocketAddr, token: Option<&str>) -> Socket {
        let stream = TcpStream::connect(addr).await.expect("connect over TCP");
        Self::open(Box::new(stream), token).await
    }

    async fn open(stream: Box<dyn Io>, token: Option<&str>) -> Socket {
        let mut request = "ws://localhost/v1/client".into_client_request().expect("a request");
        if let Some(token) = token {
            request.headers_mut().insert(AUTHORIZATION, format!("Bearer {token}").parse().expect("a header"));
        }
        let (ws, _) = client_async(request, stream).await.expect("WebSocket handshake");
        Socket { ws }
    }

    /// Sends `hello` with `protocol`; the server's answer is `welcome` or an `error`.
    pub async fn hello_with(mut self, protocol: u32) -> Result<Client, ErrorMsg> {
        let hello = ClientMsg::Hello(HelloMsg {
            protocol,
            client: ClientInfo { name: "psst-e2e".into(), version: "0".into() },
        });
        send_json(&mut self.ws, &hello).await;
        let mut client = Client { ws: self.ws, welcome: None };
        match client.recv().await {
            ServerMsg::Welcome(welcome) => {
                client.welcome = Some(welcome);
                Ok(client)
            }
            ServerMsg::Error(err) => {
                assert!(client.closed().await, "the server must close after refusing `hello`");
                Err(err)
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    pub async fn hello(self) -> Result<Client, ErrorMsg> {
        self.hello_with(psst_proto::PROTOCOL_VERSION).await
    }
}

async fn send_json(ws: &mut WebSocketStream<Box<dyn Io>>, msg: &ClientMsg) {
    let text = serde_json::to_string(msg).expect("client messages serialise");
    ws.send(Message::Text(text.into())).await.expect("send a message");
}

/// A connection after `welcome`.
pub struct Client {
    ws: WebSocketStream<Box<dyn Io>>,
    welcome: Option<WelcomeMsg>,
}

impl Client {
    /// Connects to the server's socket and says `hello`.
    pub async fn connect(server: &TestServer) -> Client {
        Socket::unix(server.socket()).await.hello().await.expect("welcome")
    }

    pub fn welcome(&self) -> &WelcomeMsg {
        self.welcome.as_ref().expect("welcomed")
    }

    pub async fn send(&mut self, msg: ClientMsg) {
        send_json(&mut self.ws, &msg).await;
    }

    pub async fn send_text(&mut self, text: &str) {
        self.ws.send(Message::Text(text.into())).await.expect("send a message");
    }

    pub async fn start(&mut self, session_id: SessionId, mode: Mode, resume: bool) {
        self.start_with(StartMsg {
            session_id,
            profile: None,
            audio: AudioFormat::default(),
            mode,
            resume,
            eval: None,
        })
        .await;
    }

    pub async fn start_with(&mut self, start: StartMsg) {
        self.send(ClientMsg::Start(start)).await;
    }

    pub async fn start_eval(&mut self, session_id: SessionId, bias: bool) {
        self.start_with(StartMsg {
            session_id,
            profile: None,
            audio: AudioFormat::default(),
            mode: Mode::Upload,
            resume: false,
            eval: Some(EvalOpts { bias }),
        })
        .await;
    }

    /// Sends `samples` samples of a quiet tone in 20 ms frames, the last one padded with zeros.
    pub async fn audio(&mut self, samples: usize) {
        let mut sent = 0;
        while sent < samples {
            let real = (samples - sent).min(FRAME_SAMPLES);
            let mut frame = Vec::with_capacity(FRAME_SAMPLES * 2);
            for i in 0..FRAME_SAMPLES {
                let value: i16 = if i < real { (((sent + i) % 40) as i16 - 20) * 50 } else { 0 };
                frame.extend_from_slice(&value.to_le_bytes());
            }
            self.ws.send(Message::Binary(frame.into())).await.expect("send audio");
            sent += real;
        }
    }

    pub async fn stop(&mut self, session_id: SessionId, samples: u64) {
        self.send(ClientMsg::Stop(StopMsg { session_id, samples })).await;
    }

    pub async fn cancel(&mut self, session_id: SessionId) {
        self.send(ClientMsg::Cancel(CancelMsg { session_id })).await;
    }

    /// Starts an upload, sends `samples` and stops.
    pub async fn upload(&mut self, session_id: SessionId, samples: usize) -> ReadyMsg {
        self.start(session_id, Mode::Upload, false).await;
        let ready = self.ready().await;
        self.audio(samples).await;
        self.stop(session_id, samples as u64).await;
        ready
    }

    /// The next message; panics if the connection closes or nothing comes in time.
    pub async fn recv(&mut self) -> ServerMsg {
        self.try_recv(MESSAGE_TIMEOUT).await.expect("a message before the timeout")
    }

    /// The next message within `timeout`, or `None`.
    pub async fn try_recv(&mut self, timeout: Duration) -> Option<ServerMsg> {
        loop {
            let msg = match tokio::time::timeout(timeout, self.ws.next()).await {
                Err(_) => return None,
                Ok(msg) => msg,
            };
            match msg {
                Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).expect("a server message")),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                other => panic!("expected a message, got {other:?}"),
            }
        }
    }

    pub async fn ready(&mut self) -> ReadyMsg {
        match self.recv().await {
            ServerMsg::Ready(ready) => ready,
            other => panic!("expected ready, got {other:?}"),
        }
    }

    /// Reads until the session's terminal message; returns it with the `progress` seen before.
    pub async fn outcome(&mut self) -> (ServerMsg, Vec<ProgressMsg>) {
        let mut progress = Vec::new();
        loop {
            match self.recv().await {
                ServerMsg::Progress(p) => progress.push(p),
                msg @ (ServerMsg::Final(_) | ServerMsg::Error(_) | ServerMsg::Cancelled(_)) => return (msg, progress),
                other => panic!("expected progress or an outcome, got {other:?}"),
            }
        }
    }

    pub async fn final_msg(&mut self) -> FinalMsg {
        match self.outcome().await.0 {
            ServerMsg::Final(fin) => *fin,
            other => panic!("expected final, got {other:?}"),
        }
    }

    pub async fn error(&mut self) -> ErrorMsg {
        match self.outcome().await.0 {
            ServerMsg::Error(err) => err,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Waits for a `progress` whose state passes `pred`.
    pub async fn progress_until(&mut self, pred: impl Fn(&ProgressMsg) -> bool) -> ProgressMsg {
        loop {
            match self.recv().await {
                ServerMsg::Progress(p) if pred(&p) => return p,
                ServerMsg::Progress(_) => {}
                other => panic!("expected progress, got {other:?}"),
            }
        }
    }

    /// Whether the server closes the connection within the message timeout.
    pub async fn closed(&mut self) -> bool {
        loop {
            match tokio::time::timeout(MESSAGE_TIMEOUT, self.ws.next()).await {
                Err(_) => return false,
                Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return true,
                Ok(Some(Ok(_))) => {}
            }
        }
    }
}

/// A minimal HTTP/1.1 exchange over TCP: returns the status and the body.
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect over TCP");
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let auth = token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("send the request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read the response");
    let response = String::from_utf8(response).expect("a UTF-8 response");
    let (head, body) = response.split_once("\r\n\r\n").expect("a complete response");
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
    let body =
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.into() };
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}
