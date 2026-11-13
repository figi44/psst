//! The `psst-server` binary: `check-config` and the `__fake-engine` mode.

use std::{
    fs,
    io::{BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

use psst_engine_proto::{
    Bias, FrameLimits, FromEngine, Load, PROTOCOL_ENV, ToEngine, Transcribe, decode, encode,
    fake::{self, Script, TranscribeStep},
    read_frame, samples_to_payload, write_frame,
};

const BIN: &str = env!("CARGO_BIN_EXE_psst-server");

fn check_config(dir: &Path) -> (bool, String) {
    let out = Command::new(BIN).arg("check-config").arg(dir).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn check_config_accepts_an_empty_directory_as_all_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, stderr) = check_config(dir.path());
    assert!(ok, "{stderr}");
}

#[test]
fn check_config_reports_invalid_files_and_dictionary_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("profiles.toml"), "[profiles.czech]\nlanguage = \"cs\"").unwrap();
    let (ok, stderr) = check_config(dir.path());
    assert!(!ok);
    assert!(stderr.contains("profiles.toml") && stderr.contains("default"), "{stderr}");

    fs::remove_file(dir.path().join("profiles.toml")).unwrap();
    fs::write(dir.path().join("dictionary.toml"), "[[terms]]\ntext = \"GPU\"\n[[terms]]\ntext = \"gpu\"").unwrap();
    let (ok, stderr) = check_config(dir.path());
    assert!(!ok);
    assert!(stderr.contains("dictionary.toml"), "{stderr}");
}

#[test]
fn check_config_prints_warnings_but_passes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("profiles.toml"), format!("[profiles.default]\nprompt_style = \"{}\"", "x".repeat(500)))
        .unwrap();
    let (ok, stderr) = check_config(dir.path());
    assert!(ok, "{stderr}");
    assert!(stderr.contains("warning"), "{stderr}");
}

#[test]
fn check_config_prints_dictionary_warnings_but_passes() {
    let dir = tempfile::tempdir().unwrap();
    let mut toml = String::from("[[terms]]\ntext = \"Go\"\n[[terms]]\ntext = \"Nix\"\nsounds_like = [\"next\"]\n");
    // 61 pinned terms of 8 bytes, joined by ", ", come to about 153 prompt tokens.
    for i in 0..61 {
        toml.push_str(&format!("[[terms]]\ntext = \"Pinned{i:02}\"\npin = true\n"));
    }
    fs::write(dir.path().join("dictionary.toml"), toml).unwrap();
    let (ok, stderr) = check_config(dir.path());
    assert!(ok, "{stderr}");
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        [
            "dictionary.toml: warning: term `Go` is a common en word: in en sessions it never changes the case of `go`, and doesn't protect it",
            "dictionary.toml: warning: term `Nix`: sounds_like `next` is a common en word: in en sessions every `next` becomes `Nix`",
            "dictionary.toml: warning: pinned terms for every language come to about 153 prompt tokens, over about 150; they crowd out the profile's style and every other term",
        ]
    );
}

#[test]
fn check_config_rejects_a_missing_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!check_config(&dir.path().join("nope")).0);
}

#[test]
fn fake_engine_needs_the_protocol_variable() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    fs::write(&script, "{}").unwrap();
    let out = Command::new(BIN).arg("__fake-engine").arg(&script).env_remove(PROTOCOL_ENV).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[test]
fn fake_engine_process_speaks_the_engine_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("script.json");
    let script = Script {
        transcribe: vec![TranscribeStep::Result {
            text: "hello there".into(),
            delay_ms: 0,
            no_speech_prob: None,
            speech_ms: None,
        }],
        ..Script::default()
    };
    fs::write(&script_path, serde_json::to_vec(&script).unwrap()).unwrap();

    let mut child = Command::new(BIN)
        .arg("__fake-engine")
        .arg(&script_path)
        .env(PROTOCOL_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next = || -> FromEngine {
        let frame = read_frame(&mut stdout, &FrameLimits::default()).unwrap().expect("engine closed stdout");
        decode(&frame.header).unwrap()
    };
    let mut send = |msg: ToEngine, payload: &[u8]| {
        write_frame(&mut stdin, &encode(&msg), payload).unwrap();
        stdin.flush().unwrap();
    };

    assert!(matches!(next(), FromEngine::Hello(hello) if hello.protocol == 1));
    send(
        ToEngine::Load(Load {
            model: "tiny".into(),
            revision: None,
            device: "cpu".into(),
            compute_type: "int8".into(),
        }),
        &[],
    );
    assert!(matches!(next(), FromEngine::Loaded(loaded) if loaded.model == "tiny"));
    send(
        ToEngine::Transcribe(Transcribe {
            id: 1,
            language: "en".into(),
            bias: Bias::default(),
            options: serde_json::json!({}),
        }),
        &samples_to_payload(&[0.0; 16_000]),
    );
    let FromEngine::Result(result) = next() else { panic!("expected a result") };
    assert_eq!(result.trace.result.segments[0].text, " hello there");
    send(ToEngine::Shutdown, &[]);
    assert!(child.wait().unwrap().success());

    let logged: Vec<_> = fake::read_request_log(&script_path).unwrap().into_iter().map(|r| r.samples).collect();
    assert_eq!(logged, [0, 16_000, 0]);
}
