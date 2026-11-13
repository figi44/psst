//! Refuses to connect anywhere but the sandbox compositor (spikes/sandbox).
//!
//! Every binary calls [`assert_sandbox`] before opening a Wayland connection.
//! Mirrors `spikes/sandbox/guard.sh`: the Wayland socket, `XDG_RUNTIME_DIR`
//! and `NIRI_SOCKET` must resolve inside `$PSST_SANDBOX_ROOT`, which must not be
//! in `/run/user`, and `WAYLAND_SOCKET` / `DISPLAY` must be unset.

use std::env;
use std::path::{Path, PathBuf};

fn refuse(msg: String) -> ! {
    eprintln!("sandbox guard: REFUSING: {msg}");
    std::process::exit(3);
}

fn resolve(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .unwrap_or_else(|e| refuse(format!("cannot resolve {}: {e}", p.display())))
}

/// Returns the sandbox root after checking the client environment; exits
/// with status 3 otherwise.
pub fn assert_sandbox() -> PathBuf {
    let root = env::var_os("PSST_SANDBOX_ROOT")
        .unwrap_or_else(|| refuse("PSST_SANDBOX_ROOT is not set".into()));
    let root = resolve(Path::new(&root));
    let uid = rustix::process::getuid().as_raw();
    let live_rt = PathBuf::from(format!("/run/user/{uid}"));
    if root.starts_with(&live_rt) || root == Path::new("/") {
        refuse(format!("sandbox root {} is not allowed", root.display()));
    }
    let inside = |name: &str, p: &Path| {
        let r = resolve(p);
        if !r.starts_with(&root) || r.starts_with(&live_rt) {
            refuse(format!(
                "{name}={} resolves to {}, outside {}",
                p.display(),
                r.display(),
                root.display()
            ));
        }
    };
    if env::var_os("WAYLAND_SOCKET").is_some() {
        refuse("WAYLAND_SOCKET is set".into());
    }
    if env::var_os("DISPLAY").is_some() {
        refuse("DISPLAY is set".into());
    }
    let rt = env::var_os("XDG_RUNTIME_DIR")
        .unwrap_or_else(|| refuse("XDG_RUNTIME_DIR is not set".into()));
    inside("XDG_RUNTIME_DIR", Path::new(&rt));
    let wd = env::var_os("WAYLAND_DISPLAY")
        .unwrap_or_else(|| refuse("WAYLAND_DISPLAY is not set".into()));
    let sock = if Path::new(&wd).is_absolute() {
        PathBuf::from(&wd)
    } else {
        Path::new(&rt).join(&wd)
    };
    inside("WAYLAND_DISPLAY", &sock);
    if let Some(ns) = env::var_os("NIRI_SOCKET") {
        inside("NIRI_SOCKET", Path::new(&ns));
    }
    for v in ["XDG_STATE_HOME", "XDG_CONFIG_HOME"] {
        let p = env::var_os(v).unwrap_or_else(|| refuse(format!("{v} is not set")));
        inside(v, Path::new(&p));
    }
    for live in ["PSST_LIVE_WAYLAND_SOCKET", "PSST_LIVE_NIRI_SOCKET"] {
        if let Some(l) = env::var_os(live).filter(|l| !l.is_empty())
            && let Ok(l) = std::fs::canonicalize(&l)
            && l == resolve(&sock)
        {
            refuse(format!(
                "WAYLAND_DISPLAY is the live socket {}",
                l.display()
            ));
        }
    }
    root
}
