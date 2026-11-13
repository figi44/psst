//! `psst-eval audio`: it runs only inside `scripts/gpu-session.sh`, and records both traces of
//! every clip through the in-process server.
//!
//! The end-to-end tests here run `audio::run` against `psst_server::serve_in_process` with the
//! fake engine and a few fixture clips, so they need no GPU. The real run, with the real engine
//! and `evals/clips`, stays local (docs/evals.md).

mod common;

use std::{fs, fs::File, path::PathBuf, process::Command, time::Duration};

use common::{CS_PROMPT, EN_PROMPT, Evals, Run};
use psst_engine_proto::{
    EngineErrorCode,
    fake::{LoadOutcome, LoadScript, Script, read_request_log},
};
use psst_eval::{
    audio::{AudioOptions, run},
    cases::{Case, Source, Split, read_cases},
};

/// `psst-eval audio` with the GPU lock in `cache` and `PSST_GPU_SESSION` as given.
fn audio(evals: &Evals, cache: &std::path::Path, session: Option<&str>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_psst-eval"));
    command.args(["audio", "--evals"]).arg(evals.path()).env("PSST_CACHE_DIR", cache).env_remove("PSST_GPU_SESSION");
    if let Some(session) = session {
        command.env("PSST_GPU_SESSION", session);
    }
    Run(command.output().unwrap())
}

#[test]
fn it_refuses_to_run_outside_a_gpu_session() {
    let evals = Evals::new();
    let cache = tempfile::tempdir().unwrap();
    for session in [None, Some("4242")] {
        let run = audio(&evals, cache.path(), session);
        assert_eq!(run.code(), 2, "{}", run.stderr());
        assert!(run.stderr().contains("runs only inside scripts/gpu-session.sh"), "{}", run.stderr());
    }
}

#[test]
fn inside_a_gpu_session_it_goes_on_to_the_eval_set() {
    let evals = Evals::new();
    let cache = tempfile::tempdir().unwrap();
    // What gpu-session.sh does: hold the lock while its command runs, and export its pid.
    let lock = File::create(cache.path().join("gpu.lock")).unwrap();
    lock.lock().unwrap();
    let run = audio(&evals, cache.path(), Some("4242"));
    assert_eq!(run.code(), 2, "{}", run.stderr());
    assert!(run.stderr().contains("has no cases"), "{}", run.stderr());
}

/// The fixture clips: 0.5 s, and 4321 samples, which isn't a whole number of 20 ms frames.
const CLIP: (&str, usize) = ("clip.flac", 8000);
const SHORT_CLIP: (&str, usize) = ("short.flac", 4321);

/// An eval set whose server runs the fake engine with `script`, and one case per `(id, clip)`;
/// the id's first two letters are its language. Returns the script's path, for its request log.
fn fake_eval_set(evals: &Evals, script: &Script, cases: &[(&str, (&str, usize))]) -> PathBuf {
    let script_path = evals.file("engine-script.json");
    fs::write(&script_path, serde_json::to_vec(script).unwrap()).unwrap();
    evals.write(
        "server.toml",
        &common::SERVER_TOML.replace(
            r#"command = ["psst-engine-dev"]"#,
            &format!(
                r#"command = ["{}", "__fake-engine", "{}"]"#,
                env!("CARGO_BIN_EXE_psst-eval"),
                script_path.display()
            ),
        ),
    );
    fs::create_dir(evals.file("clips")).unwrap();
    let cases: Vec<Case> = cases
        .iter()
        .map(|&(id, (clip, _))| {
            let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(clip);
            fs::copy(fixture, evals.file(&format!("clips/{id}.flac"))).unwrap();
            Case {
                id: id.into(),
                lang: id[..2].into(),
                source: Source::Synthetic,
                split: Split::Tune,
                reference: "fake transcript".into(),
                terms: Vec::new(),
                must_pass: false,
                engine_output: None,
                engine_output_raw: None,
            }
        })
        .collect();
    evals.set_cases(&cases);
    script_path
}

fn options(evals: &Evals, cache: &tempfile::TempDir) -> AudioOptions {
    AudioOptions { evals: evals.path().into(), cache_dir: cache.path().into(), require_gpu_session: false }
}

#[tokio::test(flavor = "multi_thread")]
async fn it_records_both_traces_of_every_clip_through_the_in_process_server() {
    let evals = Evals::new();
    let cases = [("en-1", CLIP), ("en-2", SHORT_CLIP), ("cs-1", CLIP)];
    let script = fake_eval_set(&evals, &Script::default(), &cases);
    let cache = tempfile::tempdir().unwrap();

    run(&options(&evals, &cache)).await.unwrap();

    for case in read_cases(&evals.file("cases.jsonl")).unwrap() {
        let (output, raw) = (case.engine_output.unwrap(), case.engine_output_raw.unwrap());
        // Each case is dictated with its language's profile, biased and then raw.
        let prompt = if case.lang == "cs" { CS_PROMPT } else { EN_PROMPT };
        assert_eq!((output.prompt.as_str(), raw.prompt.as_str()), (prompt, ""), "case {}", case.id);
        assert_eq!((output.result.language.as_str(), raw.result.language.as_str()), (&*case.lang, &*case.lang));
    }
    // Every clip uploaded twice, and the shortest clip of each language live, each with exactly
    // the clip's samples: the padding of the last frame never reaches the engine.
    let mut transcribed: Vec<usize> = read_request_log(&script)
        .unwrap()
        .into_iter()
        .filter(|request| request.header["type"] == "transcribe")
        .map(|request| request.samples)
        .collect();
    transcribed.sort_unstable();
    let mut expected: Vec<usize> = cases.iter().flat_map(|(_, (_, samples))| [*samples, *samples]).collect();
    expected.extend([SHORT_CLIP.1, CLIP.1]);
    expected.sort_unstable();
    assert_eq!(transcribed, expected);
    // The baseline it wrote is the replay of what it recorded.
    evals.run("replay", &["--check"]).success();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_fails_to_load_stops_it_at_once() {
    let evals = Evals::new();
    let outcome = LoadOutcome::Error { code: EngineErrorCode::LoadFailed, message: "CUDA driver too old".into() };
    let script = Script { load: LoadScript { outcome, ..LoadScript::default() }, ..Script::default() };
    fake_eval_set(&evals, &script, &[("en-1", CLIP)]);
    let cache = tempfile::tempdir().unwrap();

    // Not the half hour it waits for a model that is still loading or downloading.
    let err = tokio::time::timeout(Duration::from_secs(30), run(&options(&evals, &cache)))
        .await
        .expect("it gives up at once")
        .unwrap_err();
    assert!(format!("{err:#}").contains("CUDA driver too old"), "{err:#}");
    assert!(!evals.file("baseline.json").exists());
}
