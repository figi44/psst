//! Test fixtures for psst (plan 5.7, 12): a server with the scripted fake engine, a daemon in a
//! sandbox, and the nested-niri sandbox.
//!
//! Tests pass freshly built binaries: `psst-e2e` gives `env!("CARGO_BIN_EXE_psst-server-e2e")`
//! and `env!("CARGO_BIN_EXE_psst-e2e-daemon")`.
//!
//! [`fake_server`] is a scripted stand-in for the server, for client and daemon tests that need
//! to stall it, delay `ready` or refuse connections on demand.

pub mod fake_server;

use std::{
    ffi::{OsStr, OsString},
    fs,
    net::{SocketAddr, TcpListener},
    os::unix::{fs::PermissionsExt, net::UnixStream},
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
}

/// A `psst-server serve` process with its own directories. Killed on drop.
#[derive(Debug)]
pub struct TestServer {
    child: Option<Child>,
    bin: PathBuf,
    dir: TempDir,
    socket: PathBuf,
    tcp: Option<SocketAddr>,
}

/// Starts `bin serve` with the fake engine and waits until its socket accepts connections.
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
    server.model.command = vec![bin.display().to_string(), "__fake-engine".into(), script.display().to_string()];
    let files = ServerFiles { server, profiles: cfg.profiles, dictionary: cfg.dictionary };
    files.write(&root.join("config")).expect("write config");

    let mut server = TestServer { child: None, bin: bin.into(), dir, socket, tcp };
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
            .arg(root.join("cache"))
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
        self.dir.path().join("cache")
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

/// An isolated session for clients: its own XDG directories and, for nested niri, its own
/// compositor. Clients started through [`Sandbox::command`] never see the live session.
#[derive(Debug)]
pub struct Sandbox {
    /// Holds the sandbox's directories; removed on drop.
    pub root: TempDir,
    /// Set for every client: `WAYLAND_DISPLAY`, `NIRI_SOCKET`, `XDG_RUNTIME_DIR`,
    /// `XDG_STATE_HOME` and `XDG_CONFIG_HOME`.
    pub env: Vec<(OsString, OsString)>,
    /// The sandbox's compositor; killed on drop.
    pub compositor: Option<Child>,
}

impl Sandbox {
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_os_str())
    }

    fn dir(&self, name: &str) -> PathBuf {
        PathBuf::from(self.var(name).unwrap_or_else(|| panic!("sandbox has no {name}")))
    }

    /// A command with the sandbox's environment. `WAYLAND_SOCKET` and `DISPLAY` are cleared, and
    /// so are `WAYLAND_DISPLAY` and `NIRI_SOCKET` unless the sandbox sets them.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        isolate(&mut command, &self.env);
        command
    }

    /// A sandbox without a compositor: its own `XDG_RUNTIME_DIR`, `XDG_STATE_HOME` and
    /// `XDG_CONFIG_HOME`, and no `WAYLAND_DISPLAY` or `NIRI_SOCKET`. For daemon tests that use
    /// the `[insert] record` seam instead of Wayland.
    pub fn headless() -> Sandbox {
        let root = tempfile::Builder::new().prefix("psst-sandbox-").tempdir().expect("temp dir");
        let mut env = Vec::new();
        for (var, sub) in [("XDG_RUNTIME_DIR", "runtime"), ("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config")] {
            let dir = root.path().join(sub);
            fs::create_dir(&dir).expect("create sandbox dir");
            env.push((OsString::from(var), dir.into_os_string()));
        }
        fs::set_permissions(root.path().join("runtime"), fs::Permissions::from_mode(0o700)).expect("chmod runtime");
        Sandbox { root, env, compositor: None }
    }
}

/// Clears every variable that could reach the live session, then applies `env`.
fn isolate(command: &mut Command, env: &[(OsString, OsString)]) {
    for var in ["WAYLAND_SOCKET", "DISPLAY", "WAYLAND_DISPLAY", "NIRI_SOCKET"] {
        command.env_remove(var);
    }
    command.envs(env.iter().map(|(k, v)| (k, v)));
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(compositor) = &mut self.compositor {
            let _ = compositor.kill();
            let _ = compositor.wait();
        }
    }
}

/// Starts nested niri with `nix/test-niri.kdl` and returns its sandbox (plan 12). Refuses to run
/// if any client variable would point at the live session.
pub fn nested_niri() -> Sandbox {
    todo!("psst-testkit: nested_niri (Phase 0, Wayland work package)")
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
