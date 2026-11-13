//! The psst desktop client: the daemon (audio, recordings, delivery, niri IPC) and the CLI that
//! talks to it over `$XDG_RUNTIME_DIR/psst/daemon.sock` (plan 9).
//!
//! [`main`] is the whole `psst` program, so the thin binary and `psst-e2e`'s `psst-e2e-daemon`
//! run the same code.
//!
//! This is the contract stub: the commands parse; their behaviour arrives with the client work
//! package.

pub mod store;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use psst_proto::{Endpoint, SessionId};

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
        #[arg(long)]
        json: bool,
    },
    /// Pair this device with a server: `psst pair http://desktop:7341 <code>`.
    Pair { server: Endpoint, code: String },
}

/// The `psst` program.
pub fn main() -> ExitCode {
    let command = match Cli::parse().command {
        Command::Daemon => "daemon",
        Command::Toggle { .. } => "toggle",
        Command::Start => "start",
        Command::Stop => "stop",
        Command::Cancel => "cancel",
        Command::Retry { .. } => "retry",
        Command::PasteLast { .. } => "paste-last",
        Command::CopyLast { .. } => "copy-last",
        Command::Discard { .. } => "discard",
        Command::Status { .. } => "status",
        Command::Pair { .. } => "pair",
    };
    eprintln!("psst: `{command}` is not implemented yet");
    ExitCode::FAILURE
}
