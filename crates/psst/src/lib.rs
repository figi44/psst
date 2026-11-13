//! The psst desktop client: the daemon (audio, recordings, delivery, niri IPC) and the CLI that
//! talks to it over `$XDG_RUNTIME_DIR/psst/daemon.sock`.
//!
//! [`main`] is the whole `psst` program, so the thin binary and `psst-e2e`'s `psst-e2e-daemon`
//! run the same code. [`daemon::spawn`] runs the daemon inside another program with its
//! dependencies ([`daemon::Deps`]) swapped for tests.
//!
//! | Module | What |
//! |---|---|
//! | [`daemon`] | the state machine, connection and delivery |
//! | [`audio`] | capture (cpal, or a WAV file), resampling, the recording file |
//! | [`store`] | recordings, `last.json`, recovery and retention |
//! | [`ipc`] | the command socket |
//! | [`follow`] | `psst status --follow`, for status bars |
//! | [`ui`] | the seam to the Wayland thread |
//! | [`niri`] | the focused window, for the paste chord |
//! | [`pair`] | `psst pair` and the credentials file |
//! | [`dict`] | `psst dict`, a client of the server's dictionary API |
//! | [`web_ui`] | `psst ui`, a login link to the server's web UI |
//! | [`server_api`] | the server's API at the server `client.toml` names, for those two |

pub mod audio;
pub mod daemon;
pub mod dict;
pub mod follow;
pub mod ipc;
mod logind;
pub mod niri;
pub mod pair;
pub mod server_api;
mod sound;
pub mod store;
pub mod ui;
pub mod web_ui;

use std::{env, path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Parser, Subcommand};
use psst_config::{ClientConfig, ConfigFile};
use psst_proto::{Endpoint, SessionId};

use crate::ipc::Request;

#[derive(Debug, Parser)]
#[command(name = "psst", version, about = "Dictation for Wayland, transcribed by a psst server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon (a systemd user service).
    Daemon,
    /// Start recording, or stop and insert the text.
    Toggle {
        /// Profile for this dictation, instead of client.toml's.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Start recording.
    Start,
    /// Stop recording and insert the text.
    Stop,
    /// Cancel the current dictation and delete its recording.
    Cancel,
    /// Send the newest saved recording again.
    Retry {
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Paste the last dictation again.
    PasteLast {
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Copy the last dictation to the clipboard.
    CopyLast {
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Delete a saved recording.
    Discard {
        #[arg(long)]
        session: SessionId,
    },
    /// Show the daemon's state and saved recordings.
    Status {
        /// The full status as JSON.
        #[arg(long)]
        json: bool,
        /// Keep running for a status bar: a JSON line for waybar's `custom` module now and on
        /// every change (with --json, the full status per line).
        #[arg(long)]
        follow: bool,
    },
    /// Pair this device with a server: `psst pair http://desktop:7341 <code>`.
    Pair { server: Endpoint, code: String },
    /// Change the server's dictionary: add, remove or reset terms, never rules, and tests.
    Dict(dict::DictArgs),
    /// Open the server's web UI in the browser, logged in with a one-time link.
    Ui {
        /// Print the link (valid for a minute, for one login) instead of opening it.
        #[arg(long)]
        print: bool,
    },
}

/// The `psst` program.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Daemon => run_daemon(),
        Command::Pair { server, code } => pair(&server, &code),
        Command::Dict(args) => dict::run(args),
        Command::Ui { print } => web_ui::run(print),
        Command::Toggle { profile } => command(Request::Toggle { profile }, false),
        Command::Start => command(Request::Start, false),
        Command::Stop => command(Request::Stop, false),
        Command::Cancel => command(Request::Cancel, false),
        Command::Retry { session } => command(Request::Retry { session }, false),
        Command::PasteLast { session } => command(Request::PasteLast { session }, false),
        Command::CopyLast { session } => command(Request::CopyLast { session }, false),
        Command::Discard { session } => command(Request::Discard { session }, false),
        Command::Status { json, follow: true } => follow::main(json),
        Command::Status { json, follow: false } => command(Request::Status, json),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("psst: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `$XDG_STATE_HOME/psst`, else `~/.local/state/psst`.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| anyhow::anyhow!("neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(base.join("psst"))
}

/// `$XDG_RUNTIME_DIR/psst/daemon.sock`.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR isn't set"))?;
    Ok(PathBuf::from(runtime).join("psst").join("daemon.sock"))
}

fn command(request: Request, json: bool) -> anyhow::Result<ExitCode> {
    let socket = socket_path()?;
    let response = ipc::call(&socket, &request).map_err(|err| {
        anyhow::anyhow!("cannot reach the daemon at {} ({err}); is `psst daemon` running?", socket.display())
    })?;
    match (&response.status, json) {
        (Some(status), true) => println!("{}", serde_json::to_string_pretty(status)?),
        (Some(status), false) => print!("{}", status.render()),
        (None, _) if response.ok => println!("{}", response.message),
        (None, _) => eprintln!("psst: {}", response.message),
    }
    Ok(if response.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn pair(server: &Endpoint, code: &str) -> anyhow::Result<ExitCode> {
    let creds = pair::redeem(server, code, &pair::hostname()).map_err(anyhow::Error::msg)?;
    let dir = state_dir()?;
    store::Store::open(&dir)?;
    let path = dir.join("credentials");
    creds.save(&path)?;
    println!("paired with {server} as device {}; token saved in {}", creds.device_id, path.display());
    if let Some(config) = ClientConfig::default_path()
        && let Ok(cfg) = ClientConfig::load_or_default(&config)
        && cfg.server != *server
    {
        println!("note: client.toml uses server = \"{}\"; set it to \"{server}\" to use this pairing", cfg.server);
    }
    Ok(ExitCode::SUCCESS)
}

fn run_daemon() -> anyhow::Result<ExitCode> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("PSST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_target(false).init();

    let config_path = ClientConfig::default_path().ok_or_else(|| anyhow::anyhow!("cannot find client.toml"))?;
    let cfg = ClientConfig::load_or_default(&config_path)?;
    let paths = daemon::Paths { state_dir: state_dir()?, socket: socket_path()? };
    let wayland: Box<dyn ui::Wayland> = match &cfg.insert.record {
        Some(path) => Box::new(ui::RecordingWayland { path: path.clone() }),
        None => Box::new(ui::DesktopWayland::new(&cfg.overlay)),
    };
    let deps = daemon::Deps {
        audio: audio::source_for(&cfg.audio.input),
        wayland,
        sound: Arc::new(sound::play_error),
        niri_socket: env::var_os("NIRI_SOCKET").filter(|s| !s.is_empty()).map(PathBuf::from),
        timing: daemon::Timing::default(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let handle = daemon::spawn(cfg, paths.clone(), deps).await?;
        tracing::info!("psst daemon listening on {}", paths.socket.display());
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tracing::info!("stopping");
        handle.shutdown().await;
        let _ = std::fs::remove_file(&paths.socket);
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(ExitCode::SUCCESS)
}
