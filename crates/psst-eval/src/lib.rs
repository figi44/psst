pub mod align;
pub mod audio;
pub mod baseline;
pub mod cases;
pub mod gate;
pub mod metrics;
pub mod replay;
pub mod report;
pub mod stale;
pub mod stress;
pub mod text;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use crate::{baseline::Baseline, cases::EvalSet};

const DEFAULT_EVALS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../evals");

const REWRITE: &str = "run `cargo run -p psst-eval -- replay --write` and commit the result";

#[derive(Debug, Parser)]
#[command(name = "psst-eval", version, about = "psst regression evals")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the pipeline over the recorded engine traces, check them for staleness, and compare
    /// with a baseline (CI, no GPU).
    Replay {
        #[command(flatten)]
        evals: EvalsArg,
        /// Write the result to evals/baseline.json.
        #[arg(long)]
        write: bool,
        #[command(flatten)]
        gate: GateArgs,
    },
    /// Run the dictionary over the stress sentences in evals/stress/ (CI).
    Stress {
        #[command(flatten)]
        evals: EvalsArg,
        #[command(flatten)]
        gate: GateArgs,
    },
    /// Transcribe every clip with the worktree's server and engine, and record the traces (local,
    /// GPU, inside scripts/gpu-session.sh).
    Audio {
        #[command(flatten)]
        evals: EvalsArg,
        /// The model cache (default: ~/.cache/psst/hf).
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// How the audio goes to the server: `pcm`, as for the committed traces, or `opus`, to
        /// measure the codec on a copy of the eval set.
        #[arg(long, value_enum, default_value_t = CodecArg::Pcm)]
        codec: CodecArg,
        /// With --codec opus: the bitrate in bits per second, instead of the client's default.
        #[arg(long, value_parser = clap::value_parser!(u32).range(500..=512_000))]
        opus_bitrate: Option<u32>,
        /// With --codec opus: libopus's application, instead of the client's default.
        #[arg(long, value_enum)]
        opus_application: Option<ApplicationArg>,
    },
    /// Run the scripted test engine (see psst_engine_proto::fake).
    #[command(name = "__fake-engine", hide = true)]
    FakeEngine { script: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CodecArg {
    Pcm,
    Opus,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ApplicationArg {
    Voip,
    Audio,
}

#[derive(Debug, Args)]
struct EvalsArg {
    /// The eval set (default: this worktree's evals/).
    #[arg(long, default_value = DEFAULT_EVALS, hide_default_value = true)]
    evals: PathBuf,
}

#[derive(Debug, Args)]
struct GateArgs {
    /// Fail unless evals/baseline.json equals this run's result.
    #[arg(long)]
    check: bool,
    /// Main's baseline.json: fail on regressions against it, on the cases present in both.
    #[arg(long, value_name = "BASELINE")]
    against: Option<PathBuf>,
    /// A GitHub Actions event payload: the `baseline-change` label on its pull request lets
    /// regressions against --against pass.
    #[arg(long, value_name = "EVENT_JSON")]
    event: Option<PathBuf>,
    /// Enforce the absolute floors, which are otherwise only reported.
    #[arg(long)]
    floors: bool,
}

pub fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Replay { evals, write, gate } => replay_command(&evals.evals, write, &gate),
        Command::Stress { evals, gate } => stress_command(&evals.evals, &gate),
        Command::Audio { evals, cache_dir, codec, opus_bitrate, opus_application } => {
            codec_for(codec, opus_bitrate, opus_application)
                .and_then(|codec| audio_command(evals.evals, cache_dir, codec))
        }
        Command::FakeEngine { script } => return psst_engine_proto::fake::main(&script),
    };
    match result {
        Ok(failures) if failures.is_empty() => {
            println!("psst-eval: passed");
            ExitCode::SUCCESS
        }
        Ok(failures) => {
            for failure in &failures {
                eprintln!("FAIL: {failure}");
            }
            eprintln!("psst-eval: {} check(s) failed", failures.len());
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("psst-eval: error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn replay_command(evals: &Path, write: bool, gate: &GateArgs) -> anyhow::Result<Vec<String>> {
    let set = EvalSet::load(evals)?;
    let mut failures = Vec::new();

    let stale = stale::stale_traces(&set)?;
    for trace in &stale {
        failures.push(format!("stale trace, case {} {}: {}", trace.case, trace.trace, trace.reason));
    }
    if !stale.is_empty() {
        eprintln!(
            "{} recorded traces don't match the requests the current code and evals/*.toml make; \
             re-run the audio eval",
            stale.len()
        );
    }

    let committed = gate.check.then(|| Baseline::read(&set.baseline_path()));
    let replay = replay::replay(&set)?;
    report::print_summary(&replay.baseline);
    if !replay.unrecorded.is_empty() {
        println!(
            "{} of {} cases have no traces yet; `psst-eval audio` records them",
            replay.unrecorded.len(),
            set.cases.len()
        );
    }
    if write {
        replay.baseline.write(&set.baseline_path())?;
        println!("wrote {}", set.baseline_path().display());
    }
    if let Some(committed) = committed {
        failures.extend(consistency(committed, &replay.baseline, &set.baseline_path()));
    }

    if let Some(path) = &gate.against {
        let main = read_main(path)?;
        let comparison = gate::compare_cases(&main, &replay.baseline);
        println!(
            "gate: {} cases in both baselines, {} new, {} only on main",
            comparison.common,
            comparison.new.len(),
            comparison.removed.len()
        );
        if !comparison.removed.is_empty() {
            println!("  only on main: {}", comparison.removed.join(", "));
        }
        waive(comparison.regressions, gate, &mut failures)?;
    }
    floors(gate::case_floors(&replay.baseline), gate, &mut failures);
    Ok(failures)
}

fn stress_command(evals: &Path, gate: &GateArgs) -> anyhow::Result<Vec<String>> {
    let set = EvalSet::load(evals)?;
    let results = stress::stress(&set)?;
    report::print_stress(&results);
    let mut failures = Vec::new();
    if gate.check {
        let path = set.baseline_path();
        match Baseline::read(&path) {
            Err(err) => failures.push(format!("{err:#}; {REWRITE}")),
            Ok(None) if results.is_empty() => {}
            Ok(None) => failures.push(format!("{} is missing; {REWRITE}", path.display())),
            Ok(Some(committed)) if committed.stress == results => {}
            Ok(Some(_)) => {
                failures.push(format!("the stress results in {} don't match this run; {REWRITE}", path.display()))
            }
        }
    }
    if let Some(path) = &gate.against {
        let main = read_main(path)?;
        waive(gate::compare_stress(&main.stress, &results), gate, &mut failures)?;
    }
    floors(gate::stress_floors(&results), gate, &mut failures);
    Ok(failures)
}

fn codec_for(
    codec: CodecArg,
    bitrate: Option<u32>,
    application: Option<ApplicationArg>,
) -> anyhow::Result<psst_client::AudioCodec> {
    if codec == CodecArg::Pcm {
        anyhow::ensure!(
            bitrate.is_none() && application.is_none(),
            "--opus-bitrate and --opus-application need --codec opus"
        );
        return Ok(psst_client::AudioCodec::Pcm);
    }
    let mut settings = psst_client::OpusSettings::default();
    if let Some(bitrate) = bitrate {
        settings.bitrate = bitrate;
    }
    if let Some(application) = application {
        settings.application = match application {
            ApplicationArg::Voip => psst_client::OpusApplication::Voip,
            ApplicationArg::Audio => psst_client::OpusApplication::Audio,
        };
    }
    Ok(psst_client::AudioCodec::Opus(settings))
}

fn audio_command(
    evals: PathBuf,
    cache_dir: Option<PathBuf>,
    codec: psst_client::AudioCodec,
) -> anyhow::Result<Vec<String>> {
    // `psst_server::engine=info` shows the engine's stderr (model downloads, CUDA errors).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,psst_server::engine=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
    let opts = audio::AudioOptions {
        evals,
        cache_dir: cache_dir.unwrap_or_else(audio::default_model_cache),
        require_gpu_session: true,
        codec,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("cannot start tokio")?;
    runtime.block_on(audio::run(&opts))?;
    Ok(Vec::new())
}

fn read_main(path: &Path) -> anyhow::Result<Baseline> {
    Baseline::read(path)?.with_context(|| format!("{} doesn't exist", path.display()))
}

fn consistency(committed: anyhow::Result<Option<Baseline>>, fresh: &Baseline, path: &Path) -> Vec<String> {
    let mismatch = |what: String| vec![format!("{} {what}; {REWRITE}", path.display())];
    match committed {
        Err(err) => vec![format!("{err:#}; {REWRITE}")],
        Ok(None) if fresh.is_empty() => Vec::new(),
        Ok(None) => mismatch("is missing".into()),
        Ok(Some(committed)) if committed == *fresh => Vec::new(),
        Ok(Some(committed)) => {
            let ids: BTreeSet<&str> = committed.cases.iter().chain(&fresh.cases).map(|case| case.id.as_str()).collect();
            let changed: Vec<&str> = ids.into_iter().filter(|id| committed.case(id) != fresh.case(id)).collect();
            let mut parts = Vec::new();
            if !changed.is_empty() {
                let mut list = changed.iter().take(5).copied().collect::<Vec<_>>().join(", ");
                if changed.len() > 5 {
                    list.push_str(&format!(" and {} more", changed.len() - 5));
                }
                parts.push(format!("cases {list}"));
            }
            if committed.stress != fresh.stress {
                parts.push("the stress results".into());
            }
            if parts.is_empty() {
                parts.push("the summary".into());
            }
            mismatch(format!("doesn't match this replay ({} differ)", parts.join(" and ")))
        }
    }
}

fn waive(regressions: Vec<String>, gate: &GateArgs, failures: &mut Vec<String>) -> anyhow::Result<()> {
    let labelled = match &gate.event {
        Some(event) => gate::has_baseline_change_label(event)?,
        None => false,
    };
    match (regressions.is_empty(), labelled) {
        (true, true) => println!("gate: no regressions; the `{}` label isn't needed", gate::BASELINE_CHANGE_LABEL),
        (true, false) => println!("gate: no regressions"),
        (false, true) => {
            println!(
                "gate: the `{}` label lets {} regression(s) pass:",
                gate::BASELINE_CHANGE_LABEL,
                regressions.len()
            );
            for regression in &regressions {
                println!("  {regression}");
            }
        }
        (false, false) => failures.extend(regressions),
    }
    Ok(())
}

fn floors(floor_failures: Vec<String>, gate: &GateArgs, failures: &mut Vec<String>) {
    if gate.floors || gate::FLOORS_ENFORCED {
        failures.extend(floor_failures.into_iter().map(|f| format!("floor: {f}")));
    } else {
        for failure in floor_failures {
            println!("floor (not enforced yet): {failure}");
        }
    }
}
