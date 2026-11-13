//! `psst-eval audio`: it runs only inside `scripts/gpu-session.sh`, and records both traces of
//! every clip through the in-process server.

mod common;

use std::{fs, fs::File, process::Command};

use common::{Evals, Run};
use psst_engine_proto::fake::{Script, read_request_log};
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "psst_server::serve_in_process and psst_client are contract stubs until the server and client land"]
async fn it_records_both_traces_of_every_clip_through_the_in_process_server() {
    let evals = Evals::new();
    let script = evals.file("engine-script.json");
    fs::write(&script, serde_json::to_vec(&Script::default()).unwrap()).unwrap();
    evals.write(
        "server.toml",
        &common::SERVER_TOML.replace(
            r#"command = ["psst-engine-dev"]"#,
            &format!(r#"command = ["{}", "__fake-engine", "{}"]"#, env!("CARGO_BIN_EXE_psst-eval"), script.display()),
        ),
    );
    let ids = ["en-1", "en-2", "cs-1"];
    fs::create_dir(evals.file("clips")).unwrap();
    let cases: Vec<Case> = ids
        .iter()
        .map(|id| {
            let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clip.flac");
            fs::copy(clip, evals.file(&format!("clips/{id}.flac"))).unwrap();
            Case {
                id: (*id).into(),
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
    let cache = tempfile::tempdir().unwrap();

    run(&AudioOptions { evals: evals.path().into(), cache_dir: cache.path().into(), require_gpu_session: false })
        .await
        .unwrap();

    for case in read_cases(&evals.file("cases.jsonl")).unwrap() {
        let (output, raw) = (case.engine_output.unwrap(), case.engine_output_raw.unwrap());
        assert!(!output.prompt.is_empty() && raw.prompt.is_empty(), "case {}", case.id);
    }
    // Two uploads per clip, and two live sessions, each with the clip's 8000 samples.
    let transcribes: Vec<usize> = read_request_log(&script)
        .unwrap()
        .into_iter()
        .filter(|request| request.header["type"] == "transcribe")
        .map(|request| request.samples)
        .collect();
    assert_eq!(transcribes.len(), 2 * ids.len() + 2);
    assert!(transcribes.iter().all(|&samples| samples == 8000), "{transcribes:?}");
    // The baseline it wrote is the replay of what it recorded.
    evals.run("replay", &["--check"]).success();
}
