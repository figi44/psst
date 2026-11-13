//! Refuses any client environment that could reach the live session (plan 12, AGENTS.md).
//!
//! Every path a client uses must resolve inside the sandbox root and outside
//! `/run/user/<uid>`, the Wayland and niri sockets must not be the live ones (compared by device
//! and inode, so links and bind mounts don't slip through), the session bus is disabled, and
//! `WAYLAND_SOCKET` and `DISPLAY` aren't set. The only exemption is the nested niri's own
//! connection to its host, the sandbox's headless sway.

use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

/// Directories and sockets a client gets from the sandbox.
pub(crate) const PATH_VARS: [&str; 6] =
    ["XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "NIRI_SOCKET"];

/// Variables that reach a session; cleared for every client.
pub(crate) const CLEARED_VARS: [&str; 6] =
    ["WAYLAND_SOCKET", "DISPLAY", "SWAYSOCK", "I3SOCK", "XAUTHORITY", "HYPRLAND_INSTANCE_SIGNATURE"];

/// The live session's runtime directory.
pub(crate) fn run_user() -> PathBuf {
    PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()))
}

/// The live session's Wayland and niri sockets, from this process's environment.
pub(crate) fn live_sockets() -> Vec<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(run_user);
    let mut live = Vec::new();
    if let Some(display) = env::var_os("WAYLAND_DISPLAY").filter(|v| !v.is_empty()) {
        live.push(runtime.join(display));
    }
    if let Some(niri) = env::var_os("NIRI_SOCKET").filter(|v| !v.is_empty()) {
        live.push(PathBuf::from(niri));
    }
    live
}

/// `path` made absolute and normalised: through symlinks if it exists, lexically otherwise.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not absolute", path.display()));
    }
    if let Ok(real) = path.canonicalize() {
        return Ok(real);
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    // The parent may exist and be a link even if the file doesn't.
    match (out.parent().and_then(|p| p.canonicalize().ok()), out.file_name()) {
        (Some(parent), Some(name)) => Ok(parent.join(name)),
        _ => Ok(out),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Checks a client environment: `vars` are what the client gets (anything not listed is
/// removed, see [`CLEARED_VARS`]).
pub(crate) fn check_isolation(
    root: &Path,
    vars: &[(OsString, OsString)],
    live: &[PathBuf],
    run_user: &Path,
) -> Result<(), String> {
    let root = resolve(root)?;
    if root == Path::new("/") || root.starts_with(run_user) {
        return Err(format!("the sandbox root {} is not allowed", root.display()));
    }
    let get = |name: &str| vars.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_os_str());
    for name in CLEARED_VARS {
        if get(name).is_some() {
            return Err(format!("{name} must not be set for a client"));
        }
    }
    if get("DBUS_SESSION_BUS_ADDRESS") != Some(OsStr::new("disabled:")) {
        return Err("DBUS_SESSION_BUS_ADDRESS must be `disabled:`".into());
    }
    let inside = |name: &str, value: &OsStr| -> Result<PathBuf, String> {
        let path = resolve(Path::new(value)).map_err(|e| format!("{name}: {e}"))?;
        if !path.starts_with(&root) || path.starts_with(run_user) {
            return Err(format!(
                "{name}={} resolves to {}, outside the sandbox {}",
                Path::new(value).display(),
                path.display(),
                root.display()
            ));
        }
        Ok(path)
    };
    for name in PATH_VARS {
        if let Some(value) = get(name) {
            inside(name, value)?;
        } else if name != "NIRI_SOCKET" {
            return Err(format!("{name} is not set"));
        }
    }
    let mut sockets = Vec::new();
    if let Some(display) = get("WAYLAND_DISPLAY") {
        let runtime = get("XDG_RUNTIME_DIR").expect("checked above");
        let socket = Path::new(runtime).join(display);
        sockets.push(("WAYLAND_DISPLAY", inside("WAYLAND_DISPLAY", socket.as_os_str())?));
    }
    if let Some(niri) = get("NIRI_SOCKET") {
        sockets.push(("NIRI_SOCKET", resolve(Path::new(niri))?));
    }
    for (name, socket) in sockets {
        for l in live {
            if same_file(&socket, l) {
                return Err(format!("{name} is the live socket {}", l.display()));
            }
        }
    }
    Ok(())
}

/// For test programs started in a sandbox (`Sandbox::command`): exits with status 3 unless this
/// process's environment is the sandbox's. Call it before connecting to anything.
pub fn assert_client_env() {
    let refuse = |msg: String| -> ! {
        eprintln!("sandbox guard: REFUSING: {msg}");
        std::process::exit(3);
    };
    let Some(root) = env::var_os("PSST_SANDBOX_ROOT") else { refuse("PSST_SANDBOX_ROOT is not set".into()) };
    let mut vars: Vec<(OsString, OsString)> = env::vars_os().collect();
    vars.retain(|(k, _)| k != "PSST_LIVE_SOCKETS");
    let live: Vec<PathBuf> = env::var_os("PSST_LIVE_SOCKETS")
        .map(|v| env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect())
        .unwrap_or_default();
    if let Err(err) = check_isolation(Path::new(&root), &vars, &live, &run_user()) {
        refuse(err);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        live_dir: PathBuf,
        live: PathBuf,
        vars: Vec<(OsString, OsString)>,
        _listeners: Vec<UnixListener>,
    }

    /// A sandbox root with its sockets, and a stand-in for the live runtime dir and socket.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("root/run")).unwrap();
        let root = dir.path().join("root").canonicalize().unwrap();
        let live_dir = dir.path().join("live");
        std::fs::create_dir_all(&live_dir).unwrap();
        let live = live_dir.join("wayland-1");
        let listeners = vec![
            UnixListener::bind(root.join("run/wayland-1")).unwrap(),
            UnixListener::bind(root.join("run/niri.sock")).unwrap(),
            UnixListener::bind(&live).unwrap(),
        ];
        let p = |s: &str| OsString::from(root.join(s));
        let vars = vec![
            ("XDG_RUNTIME_DIR".into(), p("run")),
            ("WAYLAND_DISPLAY".into(), "wayland-1".into()),
            ("NIRI_SOCKET".into(), p("run/niri.sock")),
            ("XDG_STATE_HOME".into(), p("state")),
            ("XDG_CONFIG_HOME".into(), p("config")),
            ("XDG_CACHE_HOME".into(), p("cache")),
            ("XDG_DATA_HOME".into(), p("data")),
            ("DBUS_SESSION_BUS_ADDRESS".into(), "disabled:".into()),
        ];
        Fixture { _dir: dir, root, live_dir, live, vars, _listeners: listeners }
    }

    fn check(f: &Fixture, vars: &[(OsString, OsString)]) -> Result<(), String> {
        check_isolation(&f.root, vars, std::slice::from_ref(&f.live), &f.live_dir)
    }

    fn with(f: &Fixture, name: &str, value: impl Into<OsString>) -> Vec<(OsString, OsString)> {
        let mut vars = f.vars.clone();
        vars.retain(|(k, _)| k != name);
        vars.push((name.into(), value.into()));
        vars
    }

    #[test]
    fn accepts_the_sandbox_environment() {
        let f = fixture();
        check(&f, &f.vars).unwrap();
    }

    #[test]
    fn refuses_the_live_session() {
        let f = fixture();
        // The live runtime dir, an absolute live WAYLAND_DISPLAY, the live niri socket.
        assert!(check(&f, &with(&f, "XDG_RUNTIME_DIR", &f.live_dir)).is_err());
        assert!(check(&f, &with(&f, "WAYLAND_DISPLAY", &f.live)).is_err());
        assert!(check(&f, &with(&f, "NIRI_SOCKET", &f.live)).is_err());
        // A link inside the root to the live socket.
        std::os::unix::fs::symlink(&f.live, f.root.join("run/wayland-9")).unwrap();
        let err = check(&f, &with(&f, "WAYLAND_DISPLAY", "wayland-9")).unwrap_err();
        assert!(err.contains("outside the sandbox"), "{err}");
    }

    #[test]
    fn refuses_session_variables_and_paths_outside_the_root() {
        let f = fixture();
        for name in CLEARED_VARS {
            assert!(check(&f, &with(&f, name, ":0")).is_err(), "{name}");
        }
        assert!(check(&f, &with(&f, "DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus")).is_err());
        assert!(check(&f, &with(&f, "XDG_CONFIG_HOME", "/home/someone/.config")).is_err());
        assert!(check(&f, &with(&f, "XDG_STATE_HOME", f.root.join("../escape"))).is_err());
        assert!(check(&f, &with(&f, "XDG_STATE_HOME", "relative/state")).is_err());
        let mut missing = f.vars.clone();
        missing.retain(|(k, _)| k != "XDG_CONFIG_HOME");
        assert!(check(&f, &missing).is_err());
    }

    #[test]
    fn refuses_a_root_in_the_live_runtime_dir() {
        let f = fixture();
        assert!(check_isolation(&f.root, &f.vars, &[], &f.root).is_err());
        assert!(check_isolation(Path::new("/"), &f.vars, &[], &f.live_dir).is_err());
    }
}
