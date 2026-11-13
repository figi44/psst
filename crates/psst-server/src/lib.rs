mod admin;
mod api;
mod auth;
mod conn;
mod db;
mod dictionary;
mod engine;
mod listen;
mod profiles;
mod registry;
mod settings;
mod sleep;
mod state;
mod tailscale;
mod tidy;
mod web;

use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::{Parser, Subcommand};
use psst_config::{Network, ServerFiles};
use psst_dict::Dictionary;
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    db::{Db, UNLOADED_BOOT_ID},
    dictionary::Layered,
    engine::{EngineConfig, Supervisor},
    listen::{TcpSocket, UnixSocket},
    profiles::ProfileLayers,
    registry::Registry,
    settings::ServerLayers,
    state::{AppState, ConnTable, LiveConfig, LocalPolicy},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub socket: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ServerHandle {
    socket: PathBuf,
    tcp_addrs: Vec<SocketAddr>,
    owns_socket: bool,
    shutdown: watch::Sender<bool>,
    state: Arc<AppState>,
    listeners: Vec<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn tcp_addrs(&self) -> &[SocketAddr] {
        &self.tcp_addrs
    }

    pub fn reload(&self) {
        self.state.live.reload();
    }

    #[doc(hidden)]
    pub fn simulate_resume(&self) {
        self.state.engine.resume();
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
        for listener in self.listeners {
            if tokio::time::timeout(Duration::from_secs(5), listener).await.is_err() {
                tracing::warn!("a listener didn't stop within 5 s");
            }
        }
        for task in self.background {
            task.abort();
        }
        self.state.engine.shutdown().await;
        if self.owns_socket {
            let _ = fs::remove_file(&self.socket);
        }
        Ok(())
    }
}

pub async fn serve_in_process(cfg: ServeConfig) -> anyhow::Result<ServerHandle> {
    start(cfg, None).await
}

fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|id| id.trim().to_owned()).unwrap_or_default()
}

async fn start(cfg: ServeConfig, systemd: Option<std::os::unix::net::UnixListener>) -> anyhow::Result<ServerHandle> {
    let files = ServerFiles::load(&cfg.config_dir)?;
    for warning in files.warnings() {
        tracing::warn!("{warning}");
    }
    for dir in [&cfg.state_dir, &cfg.cache_dir] {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    // History and the ChatGPT sign-in live there, and the umask would leave them readable to all.
    if let Err(err) = fs::set_permissions(&cfg.state_dir, fs::Permissions::from_mode(0o700)) {
        tracing::warn!("cannot make {} private, so other users may read its history: {err}", cfg.state_dir.display());
    }
    let db = Db::open(&cfg.state_dir.join("psst.db"))?;
    let server_layers = ServerLayers::load(files.server, &db, cfg.cache_dir.clone())?;
    let server = server_layers.effective();
    let trusted = |network: &Network| Network::LOCAL_AND_TAILNET.iter().any(|safe| safe.covers(network));
    for network in server.allow_from.iter().filter(|network| !trusted(network)) {
        tracing::warn!("allow_from lets in {network}: psst has no TLS, so anyone there can read tokens and dictations");
    }
    for (id, name) in db.revoke_tailnet_devices_except(&server.auto_pair)? {
        tracing::info!("revoked device {id}: auto_pair no longer lists {name}");
    }
    let (profile_layers, profiles) = ProfileLayers::load(files.profiles, &db)?;
    let (layered, dictionary) = Layered::load(files.dictionary, &db)?;
    for warning in dictionary.warnings() {
        tracing::warn!("{warning}");
    }
    match db.prune_history(server.history_days) {
        Ok(0) => {}
        Ok(pruned) => tracing::info!("pruned {pruned} history rows older than {} days", server.history_days),
        Err(err) => tracing::warn!("cannot prune history: {err:#}"),
    }

    let unloaded = match db.get_state(UNLOADED_BOOT_ID)? {
        Some(boot) if boot == boot_id() => {
            tracing::info!("the model was unloaded on purpose during this boot; it stays unloaded");
            true
        }
        Some(_) => {
            db.clear_state(UNLOADED_BOOT_ID)?;
            false
        }
        None => false,
    };
    if !server.model.preload && !unloaded {
        tracing::info!("model.preload is off; the model stays unloaded until `psst-server model load`");
    }

    // Before the socket, so whoever waits for the socket to accept can reach TCP too.
    let mut tcp = Vec::new();
    for addr in &server.listen {
        tcp.push(listen::bind_tcp(*addr).with_context(|| format!("cannot listen on {addr}"))?);
    }
    let tcp_addrs = tcp.iter().map(TcpSocket::local_addr).collect::<Result<Vec<_>, _>>()?;

    let systemd_mode = systemd.is_some();
    let (unix, socket) = match systemd {
        Some(listener) => {
            let path = listener.local_addr()?.as_pathname().map(Path::to_owned).unwrap_or_default();
            (listener, path)
        }
        None => {
            let path = cfg.socket.clone().unwrap_or_else(|| server.socket.clone());
            // 0o666 still admits only `local_users`: each peer is checked by uid.
            let mode = if server.local_users.is_empty() { 0o600 } else { 0o666 };
            let listener =
                listen::bind_unix(&path, mode).with_context(|| format!("cannot listen on {}", path.display()))?;
            (listener, path)
        }
    };
    let unix = UnixSocket::new(unix)?;
    let own_uid = (!systemd_mode && server.local_users.is_empty()).then(|| nix::unistd::getuid().as_raw());
    if systemd_mode && server.local_users.is_empty() {
        tracing::warn!("local_users is empty, so nobody can use the socket");
    }

    let registry = Registry::new(server.limits.max_active_sessions as usize);
    let engine = Supervisor::start(
        EngineConfig::from_server(&server, cfg.cache_dir.clone()),
        server.model.preload && !unloaded,
        registry.active(),
    );
    let (shutdown, shutdown_rx) = watch::channel(false);
    let state = Arc::new(AppState {
        live: LiveConfig::new(cfg.config_dir.clone(), server_layers, (profile_layers, profiles), (layered, dictionary)),
        db,
        registry,
        engine,
        pairings: Default::default(),
        logins: Default::default(),
        conns: ConnTable::default(),
        local: LocalPolicy { users: server.local_users.clone(), own_uid },
        tcp_addrs: tcp_addrs.clone(),
        shutdown: shutdown_rx.clone(),
        tidy: Default::default(),
        server,
    });

    let app = api::router(state.clone());
    let mut listeners = vec![tokio::spawn(listen::serve(unix, app.clone(), shutdown_rx.clone()))];
    for listener in tcp {
        listeners.push(tokio::spawn(listen::serve(listener, app.clone(), shutdown_rx.clone())));
    }

    let mut background = vec![tokio::spawn(sleep::watch_resume(state.engine.clone(), shutdown_rx.clone()))];
    let maintenance = state.clone();
    background.push(tokio::spawn(async move {
        let mut sweep = tokio::time::interval(Duration::from_secs(5));
        let mut ticks: u64 = 0;
        loop {
            sweep.tick().await;
            maintenance.registry.sweep();
            ticks += 1;
            if ticks.is_multiple_of(720) {
                if let Err(err) = maintenance.db.prune_history(maintenance.live.server().history_days) {
                    tracing::warn!("cannot prune history: {err:#}");
                }
                if let Err(err) = maintenance.db.prune_web_sessions() {
                    tracing::warn!("cannot prune browser sessions: {err:#}");
                }
            }
        }
    }));

    tracing::info!(
        "psst-server {} listening on {}{}",
        env!("CARGO_PKG_VERSION"),
        socket.display(),
        tcp_addrs.iter().map(|addr| format!(" and {addr}")).collect::<String>()
    );
    Ok(ServerHandle { socket, tcp_addrs, owns_socket: !systemd_mode, shutdown, state, listeners, background })
}

#[derive(Debug, Parser)]
#[command(name = "psst-server", version, about = "The psst dictation server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        #[arg(long, default_value = "/etc/psst")]
        config_dir: PathBuf,
        #[arg(long, default_value = "/var/lib/psst")]
        state_dir: PathBuf,
        #[arg(long, default_value = "/var/cache/psst")]
        cache_dir: PathBuf,
        /// Listen on this Unix socket instead of server.toml's.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Validate server.toml, profiles.toml and dictionary.toml in a directory.
    CheckConfig { dir: PathBuf },
    /// Load, unload or show the model.
    Model {
        #[command(subcommand)]
        action: ModelAction,
        #[command(flatten)]
        connection: Connection,
    },
    /// Print a one-time pairing code for a new device, and a link with a QR code for a phone.
    Pair {
        #[arg(long)]
        name: String,
        /// The address the device reaches this server by, as http://host:port, when the one `pair`
        /// prints (the tailnet name, else the host name) is wrong.
        #[arg(long, value_parser = device_address)]
        server: Option<psst_proto::Endpoint>,
        #[command(flatten)]
        connection: Connection,
    },
    /// List or revoke paired devices.
    Devices {
        #[command(subcommand)]
        action: DevicesAction,
        #[command(flatten)]
        connection: Connection,
    },
    /// Print p50/p95 per timing field.
    Stats {
        #[command(flatten)]
        connection: Connection,
    },
    /// The dictionary's database layer.
    Dict {
        #[command(subcommand)]
        action: DictAction,
        #[command(flatten)]
        connection: Connection,
    },
    /// Profiles stored in the server's database over profiles.toml.
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
        #[command(flatten)]
        connection: Connection,
    },
    /// Print recent dictations.
    History {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[command(flatten)]
        connection: Connection,
    },
    /// Run the scripted test engine (see psst_engine_proto::fake).
    #[command(name = "__fake-engine", hide = true)]
    FakeEngine { script: PathBuf },
}

/// Admin commands are thin clients of the HTTP API over the socket.
#[derive(Debug, clap::Args)]
struct Connection {
    /// The server's Unix socket.
    #[arg(long, global = true, default_value = psst_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Print the server's answer as JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ModelAction {
    /// Load the model, downloading it if missing, and wait until it's loaded.
    Load,
    /// Unload the model until `model load` or a reboot.
    Unload,
    /// Show the model's state.
    Status,
}

#[derive(Debug, Subcommand)]
enum DictAction {
    /// Print the database layer in dictionary.toml's format, for moving it into the file
    /// (the NixOS module's settings.dictionary).
    Export,
}

#[derive(Debug, Subcommand)]
enum ProfilesAction {
    /// Print profiles.toml with the database's profiles in place of the file's, for moving into
    /// the file (the NixOS module's settings.profiles).
    Export,
}

#[derive(Debug, Subcommand)]
enum DevicesAction {
    /// List paired and local devices.
    List,
    /// Disable a device's token.
    Revoke { id: String },
}

pub fn main() -> ExitCode {
    let admin = |connection: Connection, action| admin::run(&connection.socket, connection.json, action);
    match Cli::parse().command {
        Command::Serve { config_dir, state_dir, cache_dir, socket } => {
            serve(ServeConfig { config_dir, state_dir, cache_dir, socket })
        }
        Command::CheckConfig { dir } => check_config(&dir),
        Command::FakeEngine { script } => psst_engine_proto::fake::main(&script),
        Command::Model { action, connection } => admin(
            connection,
            match action {
                ModelAction::Load => admin::Action::ModelLoad,
                ModelAction::Unload => admin::Action::ModelUnload,
                ModelAction::Status => admin::Action::ModelStatus,
            },
        ),
        Command::Pair { name, server, connection } => admin(connection, admin::Action::Pair { name, server }),
        Command::Devices { action, connection } => admin(
            connection,
            match action {
                DevicesAction::List => admin::Action::DevicesList,
                DevicesAction::Revoke { id } => admin::Action::DevicesRevoke { id },
            },
        ),
        Command::Dict { action: DictAction::Export, connection } => admin(connection, admin::Action::DictExport),
        Command::Profiles { action: ProfilesAction::Export, connection } => {
            admin(connection, admin::Action::ProfilesExport)
        }
        Command::Stats { connection } => admin(connection, admin::Action::Stats),
        Command::History { limit, connection } => admin(connection, admin::Action::History { limit }),
    }
}

fn device_address(text: &str) -> Result<psst_proto::Endpoint, String> {
    match text.parse() {
        Ok(endpoint @ psst_proto::Endpoint::Tcp { .. }) => Ok(endpoint),
        _ => Err("expected http://host[:port]".into()),
    }
}

/// systemd's soft limit, 1024 files, is a few hundred idle connections; its hard limit is far
/// higher, and only a soft limit under it is the program's to raise.
fn raise_open_files() {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) if soft < hard => {
            if let Err(err) = setrlimit(Resource::RLIMIT_NOFILE, hard, hard) {
                tracing::warn!("cannot raise the open file limit from {soft} to {hard}: {err}");
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!("cannot read the open file limit: {err}"),
    }
}

fn serve(cfg: ServeConfig) -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,zbus=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    raise_open_files();
    let systemd = listen::systemd_socket();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("psst-server: cannot start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut hangup), Ok(mut terminate), Ok(mut interrupt)) =
            (signal(SignalKind::hangup()), signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
        else {
            tracing::error!("cannot install signal handlers");
            return ExitCode::FAILURE;
        };
        let handle = match start(cfg, systemd).await {
            Ok(handle) => handle,
            Err(err) => {
                tracing::error!("{err:#}");
                return ExitCode::FAILURE;
            }
        };
        loop {
            tokio::select! {
                _ = hangup.recv() => {
                    tracing::info!("SIGHUP: reloading profiles.toml and dictionary.toml");
                    handle.reload();
                }
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
            }
        }
        tracing::info!("shutting down");
        match handle.shutdown().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!("{err:#}");
                ExitCode::FAILURE
            }
        }
    })
}

fn check_config(dir: &Path) -> ExitCode {
    if !dir.is_dir() {
        eprintln!("psst-server: {} is not a directory", dir.display());
        return ExitCode::FAILURE;
    }
    let files = match ServerFiles::load(dir) {
        Ok(files) => files,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    for warning in files.warnings() {
        eprintln!("{warning}");
    }
    let dictionary = match Dictionary::from_file(&files.dictionary) {
        Ok(dictionary) => dictionary,
        Err(err) => {
            eprintln!("dictionary.toml: {err}");
            return ExitCode::FAILURE;
        }
    };
    for warning in dictionary.warnings() {
        eprintln!("{warning}");
    }
    println!("{}: config is valid", dir.display());
    ExitCode::SUCCESS
}
