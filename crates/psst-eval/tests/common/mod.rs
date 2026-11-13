//! An eval set in a temp dir, and `psst-eval` run against it.

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use psst_eval::cases::{Case, Source, Split, write_cases};
use psst_proto::{EngineResult, EngineTrace, Segment};
use serde_json::{Value, json};
use tempfile::TempDir;

pub const SERVER_TOML: &str = r#"
eval_mode = true
listen = []

[model]
command = ["psst-engine-dev"]
model = "large-v3-turbo"

[model.options]
path = "batched"
beam_size = 5
word_timestamps = true
vad_filter = true
terms_as = "prompt"
batch_size = 8
"#;

pub const PROFILES_TOML: &str = r#"
[profiles.default]
language = "en"
prompt_style = "I'm updating the NixOS config."

[profiles.czech]
language = "cs"
prompt_style = "Upravuji konfiguraci NixOS."
"#;

pub const DICTIONARY_TOML: &str = r#"
[[terms]]
text = "psst"

[[terms]]
text = "t3code"

[[terms]]
text = "Hyprland"
pin = true
"#;

/// What the engine was asked to use for an English case with the files above: the style, then
/// the pinned term, then the others alphabetically.
pub const EN_PROMPT: &str = "I'm updating the NixOS config. Hyprland, psst, t3code.";
pub const CS_PROMPT: &str = "Upravuji konfiguraci NixOS. Hyprland, psst, t3code.";

pub fn options() -> Value {
    json!({"path": "batched", "beam_size": 5, "word_timestamps": true, "vad_filter": true, "terms_as": "prompt",
           "batch_size": 8})
}

pub struct Evals {
    dir: TempDir,
}

impl Evals {
    /// An eval set with the config files above and no cases.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("psst-eval-test-").tempdir().unwrap();
        fs::write(dir.path().join("server.toml"), SERVER_TOML).unwrap();
        fs::write(dir.path().join("profiles.toml"), PROFILES_TOML).unwrap();
        fs::write(dir.path().join("dictionary.toml"), DICTIONARY_TOML).unwrap();
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn write(&self, name: &str, contents: &str) {
        let path = self.file(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.file(name)).unwrap()
    }

    pub fn set_cases(&self, cases: &[Case]) {
        write_cases(&self.file("cases.jsonl"), cases).unwrap();
    }

    /// Runs `psst-eval <command> --evals <dir> <args>`.
    pub fn run(&self, command: &str, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_psst-eval"))
            .arg(command)
            .arg("--evals")
            .arg(self.path())
            .args(args)
            .output()
            .unwrap();
        Run(output)
    }

    /// Regenerates baseline.json, as an agent does before pushing; panics if that fails.
    pub fn write_baseline(&self) {
        self.run("replay", &["--write"]).success();
    }

    /// Keeps the current baseline.json as main's, and returns its path.
    pub fn save_main_baseline(&self) -> String {
        let main = self.file("main-baseline.json");
        fs::copy(self.file("baseline.json"), &main).unwrap();
        main.display().to_string()
    }

    /// A GitHub event payload for a pull request with these labels.
    pub fn event(&self, labels: &[&str]) -> String {
        let labels: Vec<Value> = labels.iter().map(|name| json!({"name": name})).collect();
        let path = self.file("event.json");
        fs::write(&path, json!({"action": "synchronize", "pull_request": {"number": 7, "labels": labels}}).to_string())
            .unwrap();
        path.display().to_string()
    }
}

pub struct Run(pub Output);

impl Run {
    pub fn code(&self) -> i32 {
        self.0.status.code().unwrap()
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).into_owned()
    }

    #[track_caller]
    pub fn success(&self) -> &Self {
        assert_eq!(self.code(), 0, "expected success\nstdout:\n{}\nstderr:\n{}", self.stdout(), self.stderr());
        self
    }

    /// A failed check: exit status 1, with `message` among the failures.
    #[track_caller]
    pub fn fails_with(&self, message: &str) -> &Self {
        assert_eq!(self.code(), 1, "expected a failed check\nstdout:\n{}\nstderr:\n{}", self.stdout(), self.stderr());
        assert!(self.stderr().contains(message), "expected {message:?} in stderr:\n{}", self.stderr());
        self
    }
}

/// An engine trace with one segment.
pub fn trace(lang: &str, prompt: &str, text: &str) -> EngineTrace {
    EngineTrace {
        engine: "faster-whisper".into(),
        model: "large-v3-turbo".into(),
        revision: "0123456789abcdef".into(),
        path: "batched".into(),
        options: options(),
        prompt: prompt.into(),
        result: EngineResult {
            language: lang.into(),
            speech_ms: Some(2000),
            segments: vec![Segment {
                text: format!(" {text}"),
                start_ms: 0,
                end_ms: 2000,
                no_speech_prob: Some(0.01),
                words: None,
            }],
        },
    }
}

/// A recorded English case: the engine heard `heard` with the bias, and `heard_raw` without.
pub fn case(id: &str, reference: &str, terms: &[&str], heard: &str, heard_raw: &str) -> Case {
    Case {
        id: id.into(),
        lang: "en".into(),
        source: Source::Synthetic,
        split: Split::Tune,
        reference: reference.into(),
        terms: terms.iter().map(|t| t.to_string()).collect(),
        must_pass: false,
        engine_output: Some(trace("en", EN_PROMPT, heard)),
        engine_output_raw: Some(trace("en", "", heard_raw)),
    }
}
