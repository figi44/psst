//! Test fixtures for psst: a server with the scripted fake engine, a daemon in a
//! sandbox, and the nested-niri sandbox.
//!
//! Tests pass freshly built binaries: `psst-e2e` gives `env!("CARGO_BIN_EXE_psst-server-e2e")`
//! and `env!("CARGO_BIN_EXE_psst-e2e-daemon")`.
//!
//! [`fake_server`] is a scripted stand-in for the server, for client and daemon tests that need
//! to stall it, delay `ready` or refuse connections on demand.
//!
//! Wayland tests run in a [`Sandbox`] ([`nested_niri`] or [`headless_sway`]); [`wayland`] has
//! in-process clients that stand in for apps, clipboard managers, a keyboard and a screenshot
//! tool. Nothing here ever connects to the live session.

pub mod fake_server;
pub mod guard;
mod sandbox;
mod tools;
pub mod wayland;

pub use sandbox::{Compositor, Sandbox, headless_sway, nested_niri};

use std::{
    ffi::OsString,
    fs,
    net::{SocketAddr, TcpListener},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use psst_config::{ClientConfig, ConfigFile, DictionaryFile, ProfilesFile, ServerConfig, ServerFiles};
use psst_engine_proto::fake::{self, LoggedRequest, Script};
use psst_proto::Endpoint;
use tempfile::TempDir;

/// How long `spawn_*` wait for a socket to accept connections.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`spawn_server`] writes into its config directory.
#[derive(Debug, Clone, Default)]
pub struct TestServerConfig {
    /// The fake engine's behaviour.
    pub script: Script,
    /// Written as `server.toml`, except that the testkit sets `socket`, `listen` and
    /// `model.command` (the fake engine).
    pub server: ServerConfig,
    pub profiles: ProfilesFile,
    pub dictionary: DictionaryFile,
    /// Also listen on a free TCP port on 127.0.0.1.
    pub tcp: bool,
    /// Runs a real engine instead of the fake one (`script` is then unused). For GPU smoke tests,
    /// which run only inside `scripts/gpu-session.sh`.
    pub real_engine: Option<RealEngine>,
}

/// A real engine for [`TestServerConfig::real_engine`].
#[derive(Debug, Clone)]
pub struct RealEngine {
    /// `model.command`, for example `["psst-engine-dev"]` from `nix develop .#gpu`.
    pub command: Vec<String>,
    /// The model cache (`HF_HOME`), shared across worktrees so models aren't downloaded again.
    pub model_cache: PathBuf,
}

/// A `psst-server serve` process with its own directories. Killed on drop.
#[derive(Debug)]
pub struct TestServer {
    child: Option<Child>,
    bin: PathBuf,
    dir: TempDir,
    socket: PathBuf,
    tcp: Option<SocketAddr>,
    cache: PathBuf,
}

/// Starts `bin serve` with the fake engine (or [`TestServerConfig::real_engine`]) and waits until
/// its socket accepts connections.
///
/// Panics with the server's log if it exits or doesn't come up within [`STARTUP_TIMEOUT`].
pub fn spawn_server(bin: &Path, cfg: TestServerConfig) -> TestServer {
    let dir = tempfile::Builder::new().prefix("psst-server-").tempdir().expect("temp dir");
    let root = dir.path();
    for sub in ["config", "state", "cache"] {
        fs::create_dir(root.join(sub)).expect("create test dirs");
    }
    let script = root.join("engine-script.json");
    fs::write(&script, serde_json::to_vec_pretty(&cfg.script).expect("script serialises")).expect("write script");

    let socket = root.join("client.sock");
    let tcp = cfg.tcp.then(|| SocketAddr::from(([127, 0, 0, 1], free_port())));
    let mut server = cfg.server;
    server.socket = socket.clone();
    server.listen = tcp.into_iter().collect();
    let cache = match cfg.real_engine {
        Some(engine) => {
            server.model.command = engine.command;
            engine.model_cache
        }
        None => {
            server.model.command =
                vec![bin.display().to_string(), "__fake-engine".into(), script.display().to_string()];
            root.join("cache")
        }
    };
    let files = ServerFiles { server, profiles: cfg.profiles, dictionary: cfg.dictionary };
    files.write(&root.join("config")).expect("write config");

    let mut server = TestServer { child: None, bin: bin.into(), dir, socket, tcp, cache };
    server.start();
    server
}

impl TestServer {
    fn start(&mut self) {
        let root = self.dir.path();
        let log_path = root.join("server.log");
        let log = fs::OpenOptions::new().create(true).append(true).open(&log_path).expect("open log");
        let mut child = Command::new(&self.bin)
            .arg("serve")
            .arg("--config-dir")
            .arg(root.join("config"))
            .arg("--state-dir")
            .arg(root.join("state"))
            .arg("--cache-dir")
            .arg(&self.cache)
            .arg("--socket")
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("clone log"))
            .stderr(log)
            .spawn()
            .unwrap_or_else(|err| panic!("cannot start {}: {err}", self.bin.display()));
        wait_for_socket(&mut child, &self.socket, &log_path);
        self.child = Some(child);
    }

    /// Kills the server and starts it again with the same directories, config and engine script.
    /// The log keeps growing in the same file.
    pub fn restart(&mut self) {
        self.kill();
        self.start();
    }

    /// Waits until the server's log contains `needle` at least `count` times; panics with the
    /// log after `timeout`.
    pub fn wait_for_log(&self, needle: &str, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let log = self.log();
            if log.matches(needle).count() >= count {
                return;
            }
            if Instant::now() > deadline {
                panic!("`{needle}` didn't appear {count} times in the server log within {timeout:?}; log:\n{log}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The server's Unix socket as a client endpoint.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    /// The TCP address, when started with `tcp: true`.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp
    }

    /// The TCP endpoint, when started with `tcp: true`.
    pub fn tcp_endpoint(&self) -> Option<Endpoint> {
        self.tcp.map(|addr| Endpoint::Tcp { host: addr.ip().to_string(), port: addr.port() })
    }

    pub fn config_dir(&self) -> PathBuf {
        self.dir.path().join("config")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache.clone()
    }

    /// Every request the fake engine received, across engine restarts.
    pub fn engine_requests(&self) -> Vec<LoggedRequest> {
        fake::read_request_log(&self.dir.path().join("engine-script.json")).expect("read engine log")
    }

    /// The server's stdout and stderr so far.
    pub fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("server.log")).unwrap_or_default()
    }

    /// The running server's process id.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("the server is running").id()
    }

    /// Kills the server and waits for it.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Clears every variable that could reach the live session, then applies `env`.
pub(crate) fn isolate(command: &mut Command, env: &[(OsString, OsString)]) {
    for var in guard::CLEARED_VARS.iter().chain(&["WAYLAND_DISPLAY", "NIRI_SOCKET"]) {
        command.env_remove(var);
    }
    command.envs(env.iter().map(|(k, v)| (k, v)));
}

/// What [`spawn_daemon`] writes as `client.toml`.
#[derive(Debug, Clone)]
pub struct TestClientConfig {
    /// Written as `client.toml`, except that the testkit sets `sounds = false` and
    /// `audio.input` to [`TestClientConfig::wav`].
    pub client: ClientConfig,
    /// The recording every dictation replays: a 16 kHz mono 16-bit WAV.
    pub wav: PathBuf,
}

impl TestClientConfig {
    pub fn new(server: Endpoint, wav: impl Into<PathBuf>) -> Self {
        Self { client: ClientConfig { server, ..ClientConfig::default() }, wav: wav.into() }
    }
}

/// A `psst daemon` process in a sandbox. Killed on drop.
#[derive(Debug)]
pub struct TestDaemon {
    child: Child,
    bin: PathBuf,
    command_env: Vec<(OsString, OsString)>,
    state_dir: PathBuf,
    log: PathBuf,
}

/// Starts `bin daemon` in the sandbox and waits until its command socket accepts connections.
///
/// Panics with the daemon's log if it exits or doesn't come up within [`STARTUP_TIMEOUT`].
pub fn spawn_daemon(bin: &Path, sandbox: &Sandbox, cfg: TestClientConfig) -> TestDaemon {
    let mut client = cfg.client;
    client.sounds = false;
    client.audio.input = psst_config::AudioInput::File(cfg.wav);
    let config_dir = sandbox.dir("XDG_CONFIG_HOME").join("psst");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(config_dir.join(ClientConfig::FILE_NAME), client.to_toml_string()).expect("write client.toml");

    let log = sandbox.root.path().join("daemon.log");
    let log_file = fs::File::create(&log).expect("create log");
    let mut child = sandbox
        .command(bin)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log_file.try_clone().expect("clone log"))
        .stderr(log_file)
        .spawn()
        .unwrap_or_else(|err| panic!("cannot start {}: {err}", bin.display()));
    wait_for_socket(&mut child, &sandbox.dir("XDG_RUNTIME_DIR").join("psst/daemon.sock"), &log);
    TestDaemon {
        child,
        bin: bin.into(),
        command_env: sandbox.env.clone(),
        state_dir: sandbox.dir("XDG_STATE_HOME").join("psst"),
        log,
    }
}

impl TestDaemon {
    /// Runs `psst <args>` (for example `toggle`) against this daemon and waits for it.
    pub fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.bin);
        isolate(&mut command, &self.command_env);
        command.args(args).output().unwrap_or_else(|err| panic!("cannot run {}: {err}", self.bin.display()))
    }

    /// `$XDG_STATE_HOME/psst`: recordings, `last.json`, credentials.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The daemon's stdout and stderr so far.
    pub fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Kills the daemon and waits for it.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A TCP port that was free a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("bind a free port").port()
}

fn wait_for_socket(child: &mut Child, socket: &Path, log: &Path) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        let log_text = || fs::read_to_string(log).unwrap_or_default();
        if let Ok(Some(status)) = child.try_wait() {
            panic!("process exited with {status} before {} came up; log:\n{}", socket.display(), log_text());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{} didn't come up within {STARTUP_TIMEOUT:?}; log:\n{}", socket.display(), log_text());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Writes `samples` as a 16 kHz mono 16-bit WAV file, like the daemon's recordings.
pub fn write_wav(path: &Path, samples: &[i16]) {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&16_000u32.to_le_bytes());
    bytes.extend_from_slice(&32_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).unwrap_or_else(|err| panic!("cannot write {}: {err}", path.display()));
}

/// `ms` milliseconds of distinct, recognisable 16 kHz audio: a ramp that never repeats within
/// 4 s, so order and gaps show up when comparing.
pub fn test_audio(ms: u32) -> Vec<i16> {
    (0..ms as usize * 16).map(|i| ((i % 64_000) as i32 - 32_000) as i16).collect()
}
