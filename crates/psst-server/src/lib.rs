//! The psst server: listeners, sessions, the engine supervisor, the pipeline and the HTTP API.
//!
//! [`main`] is the whole `psst-server` program, so the thin binary and `psst-e2e`'s
//! `psst-server-e2e` run the same code. [`serve_in_process`] runs the server inside another
//! program (`psst-eval`).
//!
//! This is the contract stub: `check-config` and `__fake-engine` work; `serve` and the admin
//! commands arrive with the server work package.

mod db;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use psst_config::ServerFiles;
use psst_dict::Dictionary;

/// What `psst-server serve` takes on its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    /// Holds `server.toml`, `profiles.toml` and `dictionary.toml`.
    pub config_dir: PathBuf,
    /// The database.
    pub state_dir: PathBuf,
    /// Model downloads (`HF_HOME` for the engine).
    pub cache_dir: PathBuf,
    /// Overrides `server.toml`'s `socket`.
    pub socket: Option<PathBuf>,
}

/// A running in-process server.
#[derive(Debug)]
pub struct ServerHandle {
    _private: (),
}

impl ServerHandle {
    /// The Unix socket it listens on.
    pub fn socket(&self) -> &Path {
        todo!("psst-server: ServerHandle::socket")
    }

    /// The TCP addresses it listens on, with the actual ports.
    pub fn tcp_addrs(&self) -> &[SocketAddr] {
        todo!("psst-server: ServerHandle::tcp_addrs")
    }

    /// Stops listening, ends sessions and shuts the engine down.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        todo!("psst-server: ServerHandle::shutdown")
    }
}

/// Starts the server inside this process; it runs until [`ServerHandle::shutdown`].
pub async fn serve_in_process(cfg: ServeConfig) -> anyhow::Result<ServerHandle> {
    let _ = cfg;
    todo!("psst-server: serve_in_process")
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
    /// Print a one-time pairing code for a new device.
    Pair {
        #[arg(long)]
        name: String,
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
    #[arg(long, default_value = psst_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
}

#[derive(Debug, Subcommand)]
enum ModelAction {
    /// Load the model, downloading it if missing.
    Load,
    /// Unload the model until `model load` or a reboot.
    Unload,
    /// Show the model's state.
    Status,
}

#[derive(Debug, Subcommand)]
enum DevicesAction {
    /// List paired and local devices.
    List,
    /// Disable a device's token.
    Revoke { id: String },
}

/// The `psst-server` program.
pub fn main() -> ExitCode {
    match Cli::parse().command {
        Command::CheckConfig { dir } => check_config(&dir),
        Command::FakeEngine { script } => psst_engine_proto::fake::main(&script),
        Command::Serve { .. } => not_implemented("serve"),
        Command::Model { .. } => not_implemented("model"),
        Command::Pair { .. } => not_implemented("pair"),
        Command::Devices { .. } => not_implemented("devices"),
        Command::Stats { .. } => not_implemented("stats"),
        Command::History { .. } => not_implemented("history"),
    }
}

fn not_implemented(command: &str) -> ExitCode {
    eprintln!("psst-server: `{command}` is not implemented yet");
    ExitCode::FAILURE
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
    if let Err(err) = Dictionary::from_file(&files.dictionary) {
        eprintln!("dictionary.toml: {err}");
        return ExitCode::FAILURE;
    }
    println!("{}: config is valid", dir.display());
    ExitCode::SUCCESS
}
