//! `psst-eval`: the replay, stress and audio runners (plan 7.3). The audio runner starts the
//! worktree's server in-process with `psst_server::serve_in_process`.
//!
//! This is the contract stub; the runners arrive with the eval work package.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "psst-eval", version, about = "psst regression evals")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the pipeline over recorded engine traces and compare with the baseline (CI, no GPU).
    Replay,
    /// Run the dictionary over the stress sentences (CI, Phase 2).
    Stress,
    /// Transcribe every clip with the worktree's server and engine (local, GPU).
    Audio,
}

fn main() -> ExitCode {
    let command = match Cli::parse().command {
        Command::Replay => "replay",
        Command::Stress => "stress",
        Command::Audio => "audio",
    };
    eprintln!("psst-eval: `{command}` is not implemented yet");
    ExitCode::FAILURE
}
