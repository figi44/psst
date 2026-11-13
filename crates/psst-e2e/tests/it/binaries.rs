//! The end-to-end binaries are the real programs: `psst-server-e2e` runs `psst_server::main` and
//! `psst-e2e-daemon` runs `psst::main`. Tests start them through `psst-testkit`.

use std::{fs, process::Command};

const SERVER: &str = env!("CARGO_BIN_EXE_psst-server-e2e");
const DAEMON: &str = env!("CARGO_BIN_EXE_psst-e2e-daemon");

#[test]
fn server_binary_validates_config() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("server.toml"), "listen = []\n[model]\ncommand = [\"psst-engine-faster-whisper\"]")
        .unwrap();
    let out = Command::new(SERVER).arg("check-config").arg(dir.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    fs::write(dir.path().join("server.toml"), "[model]\ncommand = []").unwrap();
    let out = Command::new(SERVER).arg("check-config").arg(dir.path()).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn daemon_binary_is_the_psst_cli() {
    let out = Command::new(DAEMON).arg("--help").output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for command in ["daemon", "toggle", "retry", "paste-last", "copy-last", "discard", "status", "pair"] {
        assert!(help.contains(command), "missing `{command}` in:\n{help}");
    }
}
