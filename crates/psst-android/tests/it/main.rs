mod contract;
mod delivery;
mod dictation;
mod history;
mod link;

use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use psst_android::{Command, Core, Dictation, Event, HistoryItem, Link, SavedDictation, SessionId, State, Timing};
use psst_proto::FRAME_SAMPLES;
use psst_testkit::fake_server::{FakeServer, FakeServerConfig};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(5);

fn timing() -> Timing {
    Timing {
        connect_budget: Duration::from_secs(2),
        start_budget: Duration::from_millis(300),
        redeliver_window: Duration::from_millis(400),
        attempt_budget: Duration::from_millis(200),
        first_backoff: Duration::from_millis(20),
        history_budget: Duration::from_millis(500),
        ..Timing::default()
    }
}

fn tcp_server(cfg: FakeServerConfig) -> FakeServer {
    FakeServer::start(FakeServerConfig { tcp: true, ..cfg })
}

fn address(server: &FakeServer) -> String {
    server.tcp_endpoint().expect("a TCP server").to_string()
}

/// The app's side of a core: what it sends, and every event it got.
struct Phone {
    core: Core,
    events: Vec<Event>,
    path: PathBuf,
    _dir: Option<TempDir>,
}

impl Phone {
    fn new() -> Phone {
        let dir = tempfile::tempdir().unwrap();
        let mut phone = Phone::at(dir.path());
        phone._dir = Some(dir);
        phone
    }

    fn at(dir: &Path) -> Phone {
        Phone { core: Core::with_timing(dir, "Test phone", timing()), events: Vec::new(), path: dir.into(), _dir: None }
    }

    /// The core's data directory.
    fn dir(&self) -> &Path {
        &self.path
    }

    fn online(server: &str) -> Phone {
        let mut phone = Phone::new();
        phone.send(Command::Configure { server: server.into() });
        phone.until_link(Link::Online);
        phone
    }

    fn send(&self, command: Command) {
        self.core.send(command);
    }

    /// In uneven chunks, as the recorder may deliver them.
    fn speak(&self, audio: &[i16]) {
        for chunk in audio.chunks(1_234) {
            self.core.audio(chunk);
        }
    }

    /// Takes events until one passes `check`, and returns those it took.
    fn until(&mut self, what: &str, check: impl Fn(&Event) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + WAIT;
        let mut taken = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(event) = self.core.next_event_timeout(left) else {
                panic!("no {what} within {WAIT:?}; got {taken:#?}");
            };
            self.events.push(event.clone());
            taken.push(event);
            if check(taken.last().unwrap()) {
                return taken;
            }
        }
    }

    fn until_state(&mut self, what: &str, check: impl Fn(&State) -> bool) -> State {
        let events = self.until(what, |event| matches!(event, Event::State { state } if check(state)));
        match events.last() {
            Some(Event::State { state }) => state.clone(),
            _ => unreachable!("`until` returns the event that passed"),
        }
    }

    fn until_link(&mut self, link: Link) -> State {
        self.until_state(&format!("{link:?}"), |state| state.link == link)
    }

    /// As the keyboard opening does, until a server that's coming back answers.
    fn warm_until_online(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let link = self.events.iter().rev().find_map(|event| match event {
                Event::State { state } => Some(state.link),
                _ => None,
            });
            match link {
                Some(Link::Online) => return,
                None | Some(Link::Connecting) => {
                    self.until_state("an answer", |state| state.link != Link::Connecting);
                }
                Some(_) => {
                    thread::sleep(Duration::from_millis(20));
                    self.send(Command::Warm);
                    self.until_link(Link::Connecting);
                }
            }
            assert!(Instant::now() < deadline, "the server never answered");
        }
    }

    fn until_dictation(&mut self, dictation: Dictation) -> State {
        self.until_state(&format!("{dictation:?}"), |state| state.dictation == dictation)
    }

    fn until_final(&mut self) -> (SessionId, String) {
        let events = self.until("final", |event| matches!(event, Event::Final { .. }));
        match events.last() {
            Some(Event::Final { session_id, text }) => (*session_id, text.clone()),
            _ => unreachable!("`until` returns the event that passed"),
        }
    }

    fn until_saved(&mut self) -> SessionId {
        let events = self.until("saved", |event| matches!(event, Event::Saved { .. }));
        match events.last() {
            Some(Event::Saved { session_id }) => *session_id,
            _ => unreachable!("`until` returns the event that passed"),
        }
    }

    fn until_saved_list(&mut self, what: &str, check: impl Fn(&[SavedDictation]) -> bool) -> Vec<SavedDictation> {
        self.until_state(what, |state| check(&state.saved)).saved
    }

    fn until_history(&mut self) -> (Vec<HistoryItem>, Option<String>) {
        let events = self.until("history", |event| matches!(event, Event::History { .. }));
        match events.last() {
            Some(Event::History { items, error }) => (items.clone(), error.clone()),
            _ => unreachable!("`until` returns the event that passed"),
        }
    }

    fn finals(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Final { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn histories(&self) -> Vec<(Vec<HistoryItem>, Option<String>)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::History { items, error } => Some((items.clone(), error.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every dictation state the app was shown, in order.
    fn dictations(&self) -> Vec<Dictation> {
        let mut shown: Vec<Dictation> = Vec::new();
        for event in &self.events {
            if let Event::State { state } = event
                && shown.last() != Some(&state.dictation)
            {
                shown.push(state.dictation);
            }
        }
        shown
    }
}

/// Waits until the server has seen `count` sessions start: audio is going out live.
fn streaming(server: &FakeServer, count: usize) {
    assert!(server.wait_until(WAIT, |s| s.sessions().len() >= count), "no live session on the server");
}

fn padded(audio: &[i16]) -> Vec<i16> {
    let mut padded = audio.to_vec();
    padded.resize(audio.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES, 0);
    padded
}

/// Between the phone and a fake server: it can refuse pairing by tailnet identity, as a server
/// whose `auto_pair` doesn't list the phone, and send new connections to another server.
struct Proxy {
    address: String,
    upstream: Arc<Mutex<SocketAddr>>,
    auto_pair: Arc<AtomicBool>,
}

impl Proxy {
    fn to(server: &FakeServer) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let upstream = Arc::new(Mutex::new(socket_addr(server)));
        let auto_pair = Arc::new(AtomicBool::new(true));
        let (to, auto) = (upstream.clone(), auto_pair.clone());
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (upstream, auto_pair) = (*to.lock().unwrap(), auto.load(Ordering::SeqCst));
                thread::spawn(move || relay(client, upstream, auto_pair));
            }
        });
        Proxy { address, upstream, auto_pair }
    }

    fn refuse_auto_pair(&self) {
        self.auto_pair.store(false, Ordering::SeqCst);
    }

    fn point_at(&self, server: &FakeServer) {
        *self.upstream.lock().unwrap() = socket_addr(server);
    }
}

fn socket_addr(server: &FakeServer) -> SocketAddr {
    address(server).trim_start_matches("http://").parse().unwrap()
}

fn relay(mut client: TcpStream, upstream: SocketAddr, auto_pair: bool) -> io::Result<()> {
    const AUTO_PAIR: &[u8] = b"POST /api/pair/auto ";
    let mut head = Vec::new();
    let mut buf = [0; 4096];
    while head.len() < AUTO_PAIR.len() || (head.starts_with(AUTO_PAIR) && !head.windows(4).any(|w| w == b"\r\n\r\n")) {
        match client.read(&mut buf)? {
            0 => return Ok(()),
            n => head.extend_from_slice(&buf[..n]),
        }
    }
    if !auto_pair && head.starts_with(AUTO_PAIR) {
        let body = r#"{"code":"forbidden","message":"the phone isn't in the server's auto_pair"}"#;
        let len = body.len();
        return write!(client, "HTTP/1.1 403 Forbidden\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{body}");
    }
    let mut server = TcpStream::connect(upstream)?;
    server.set_nodelay(true)?;
    client.set_nodelay(true)?;
    server.write_all(&head)?;
    let (mut from_client, mut to_server) = (client.try_clone()?, server.try_clone()?);
    thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_server);
        let _ = to_server.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut server, &mut client);
    client.shutdown(Shutdown::Write)
}
