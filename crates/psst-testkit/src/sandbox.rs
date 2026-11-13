//! The offscreen Wayland sandbox (plan 12; `docs/decisions/desktop.md` §1).
//!
//! [`nested_niri`] starts headless sway (`WLR_BACKENDS=headless`, pixman) in a private runtime
//! directory, then niri nested inside it with `nix/test-niri.kdl`, rendering with mesa's
//! llvmpipe. Nothing opens a window on the desktop and nothing touches the GPU.
//! [`headless_sway`] stops at the sway. Every sandbox has its own directories, so tests run in
//! parallel without a lock.
//!
//! - The compositors start from an empty environment: nothing from the live session reaches
//!   them, and the session bus is disabled.
//! - Clients get `WAYLAND_DISPLAY`, `NIRI_SOCKET`, `XDG_RUNTIME_DIR`, `XDG_STATE_HOME`,
//!   `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_DATA_HOME` inside the sandbox, with
//!   `WAYLAND_SOCKET`, `DISPLAY` and the other session variables cleared. The guard
//!   ([`crate::guard`]) refuses the sandbox if any of them could reach the live session, and
//!   runs again before every [`Sandbox::command`].
//! - In-process clients connect to [`Sandbox::wayland_socket`] by path; they never read this
//!   process's `WAYLAND_DISPLAY`, which may well be the live session's.
//!
//! `PSST_SANDBOX_KEEP=1` keeps the sandbox directory (with the compositors' logs) after the test.

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use crate::{guard, tools::tools};

/// The test niri config (plan 12): spawns nothing, no XWayland, layouts `us,cz(qwerty)`.
const NIRI_CONFIG: &str = include_str!("../../../nix/test-niri.kdl");
/// Headless sway: one output, no bar, no binds, no XWayland. No borders or title bar, so the
/// nested niri's window keeps the size it starts with (1280×800) instead of being resized just
/// after it appears.
const SWAY_CONFIG: &str = "xwayland disable\noutput HEADLESS-1 resolution 1280x800\ndefault_border none\n";

const STARTUP: Duration = Duration::from_secs(15);

/// The compositor clients talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    /// niri nested in headless sway.
    NestedNiri,
    HeadlessSway,
    /// Directories only, no Wayland.
    None,
}

/// An isolated session for clients: its own XDG directories and, unless created with
/// [`Sandbox::headless`], its own compositor. Clients started through
/// [`Sandbox::command`] never see the live session. Dropping it stops the compositors and removes
/// the directory.
#[derive(Debug)]
pub struct Sandbox {
    /// Holds the sandbox's directories.
    pub root: TempDir,
    /// What every client gets: `WAYLAND_DISPLAY`, `NIRI_SOCKET`, `XDG_RUNTIME_DIR`,
    /// `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`, a disabled session
    /// bus, and `PSST_SANDBOX_*` for test programs.
    pub env: Vec<(OsString, OsString)>,
    compositor: Compositor,
    /// Headless sway; the nested niri's host.
    host: Option<Child>,
    niri: Option<Child>,
    wayland_socket: Option<PathBuf>,
    host_socket: Option<PathBuf>,
    niri_socket: Option<PathBuf>,
    live: Vec<PathBuf>,
}

/// Starts nested niri 26.04 with `nix/test-niri.kdl` inside headless sway and returns its sandbox
/// (plan 12). Panics if it can't start, or if any client variable would point at the live
/// session.
pub fn nested_niri() -> Sandbox {
    Sandbox::start(Compositor::NestedNiri).unwrap_or_else(|err| panic!("nested niri sandbox: {err}"))
}

/// Starts headless sway alone (the daemon's end-to-end compositor in CI; no niri IPC).
pub fn headless_sway() -> Sandbox {
    Sandbox::start(Compositor::HeadlessSway).unwrap_or_else(|err| panic!("headless sway sandbox: {err}"))
}

impl Sandbox {
    /// The sandbox's directories and environment without a compositor: no `WAYLAND_DISPLAY` or
    /// `NIRI_SOCKET`. For daemon tests that use the `[insert] record` seam instead of Wayland.
    pub fn headless() -> Sandbox {
        Sandbox::start(Compositor::None).unwrap_or_else(|err| panic!("sandbox: {err}"))
    }

    fn start(compositor: Compositor) -> Result<Sandbox, String> {
        let live = guard::live_sockets();
        let base = std::env::var_os("PSST_SANDBOX_TMP").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
        let root = tempfile::Builder::new()
            .prefix("psst-sbx.")
            .tempdir_in(&base)
            .map_err(|e| format!("cannot create a sandbox in {}: {e}", base.display()))?;
        let dir = root.path().canonicalize().map_err(|e| e.to_string())?;
        // Unix socket paths must stay under 108 bytes.
        if dir.as_os_str().len() > 60 {
            return Err(format!("{} is too long for socket paths; set PSST_SANDBOX_TMP", dir.display()));
        }
        for sub in ["host", "run", "state", "config", "cache", "data", "home", "logs"] {
            fs::create_dir(dir.join(sub)).map_err(|e| e.to_string())?;
        }
        for private in ["host", "run"] {
            fs::set_permissions(dir.join(private), fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        }
        let mut sandbox = Sandbox {
            root,
            env: Vec::new(),
            compositor,
            host: None,
            niri: None,
            wayland_socket: None,
            host_socket: None,
            niri_socket: None,
            live,
        };
        if compositor != Compositor::None {
            sandbox.start_compositors(&dir)?;
        }
        let client_runtime = match &sandbox.wayland_socket {
            Some(socket) => socket.parent().expect("socket in a directory").to_path_buf(),
            None => dir.join("run"),
        };
        let mut env: Vec<(OsString, OsString)> = vec![
            ("XDG_RUNTIME_DIR".into(), client_runtime.into()),
            ("XDG_STATE_HOME".into(), dir.join("state").into()),
            ("XDG_CONFIG_HOME".into(), dir.join("config").into()),
            ("XDG_CACHE_HOME".into(), dir.join("cache").into()),
            ("XDG_DATA_HOME".into(), dir.join("data").into()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), "disabled:".into()),
            ("NO_AT_BRIDGE".into(), "1".into()),
            ("PSST_SANDBOX_ROOT".into(), dir.clone().into()),
        ];
        if let Some(socket) = &sandbox.wayland_socket {
            env.push(("WAYLAND_DISPLAY".into(), socket.file_name().expect("socket name").into()));
        }
        if let Some(niri) = &sandbox.niri_socket {
            env.push(("NIRI_SOCKET".into(), niri.into()));
        }
        if let Some(host) = &sandbox.host_socket {
            env.push(("PSST_SANDBOX_HOST_SOCKET".into(), host.into()));
        }
        let live = std::env::join_paths(&sandbox.live).unwrap_or_default();
        env.push(("PSST_LIVE_SOCKETS".into(), live));
        sandbox.env = env;
        sandbox.check()?;
        Ok(sandbox)
    }

    fn start_compositors(&mut self, dir: &Path) -> Result<(), String> {
        let tools = tools();
        fs::write(dir.join("sway.conf"), SWAY_CONFIG).map_err(|e| e.to_string())?;
        fs::write(dir.join("niri.kdl"), NIRI_CONFIG).map_err(|e| e.to_string())?;
        let mut sway = compositor_command(&tools.sway, dir, "sway.log")?;
        sway.arg("-c")
            .arg(dir.join("sway.conf"))
            .env("XDG_RUNTIME_DIR", dir.join("host"))
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1");
        let mut host = sway.spawn().map_err(|e| format!("cannot start {}: {e}", tools.sway.display()))?;
        let host_socket = wait_for_socket(&dir.join("host"), "wayland-", &mut host, dir, "sway.log");
        self.host = Some(host);
        let host_socket = host_socket?;
        self.host_socket = Some(host_socket.clone());
        if self.compositor == Compositor::HeadlessSway {
            self.wayland_socket = Some(host_socket);
            return Ok(());
        }
        // The nested niri's own connection to its host is the guard's one exemption: an absolute
        // path to the sandbox's sway, never the live display.
        let mut niri = compositor_command(&tools.niri, dir, "niri.log")?;
        niri.arg("-c")
            .arg(dir.join("niri.kdl"))
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("WAYLAND_DISPLAY", &host_socket)
            .env("__EGL_VENDOR_LIBRARY_FILENAMES", &tools.mesa_egl_vendor)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("GALLIUM_DRIVER", "llvmpipe")
            .env("LP_NUM_THREADS", "2")
            .env("RUST_LOG", std::env::var_os("PSST_NIRI_LOG").unwrap_or_else(|| "niri=warn".into()));
        let mut child = niri.spawn().map_err(|e| format!("cannot start {}: {e}", tools.niri.display()))?;
        let sockets = wait_for_socket(&dir.join("run"), "wayland-", &mut child, dir, "niri.log")
            .and_then(|w| Ok((w, wait_for_socket(&dir.join("run"), "niri.", &mut child, dir, "niri.log")?)));
        let pid = child.id();
        self.niri = Some(child);
        let (wayland, niri_socket) = sockets?;
        no_gpu(pid)?;
        self.wayland_socket = Some(wayland);
        self.niri_socket = Some(niri_socket);
        Ok(())
    }

    /// Runs the guard on this sandbox's client environment.
    fn check(&self) -> Result<(), String> {
        let root = self.root.path();
        if let Some(host) = &self.host_socket
            && !host.starts_with(root.canonicalize().map_err(|e| e.to_string())?)
        {
            return Err(format!("the host socket {} is outside the sandbox", host.display()));
        }
        let vars: Vec<(OsString, OsString)> =
            self.env.iter().filter(|(k, _)| !k.to_string_lossy().starts_with("PSST_")).cloned().collect();
        guard::check_isolation(root, &vars, &self.live, &guard::run_user())
    }

    /// Panics unless the client environment is isolated from the live session.
    pub fn assert_isolated(&self) {
        if let Err(err) = self.check() {
            panic!("sandbox guard: REFUSING: {err}");
        }
    }

    pub fn compositor(&self) -> Compositor {
        self.compositor
    }

    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_os_str())
    }

    pub(crate) fn dir(&self, name: &str) -> PathBuf {
        PathBuf::from(self.var(name).unwrap_or_else(|| panic!("sandbox has no {name}")))
    }

    /// The compositor's socket, for in-process clients (absolute; connect to it by path).
    pub fn wayland_socket(&self) -> &Path {
        self.assert_isolated();
        self.wayland_socket.as_deref().expect("this sandbox has no compositor")
    }

    /// The headless sway's socket: niri's host, where "physical" keys come from.
    pub fn host_socket(&self) -> &Path {
        self.host_socket.as_deref().expect("this sandbox has no compositor")
    }

    /// A command with the sandbox's environment, and `WAYLAND_SOCKET`, `DISPLAY` and the other
    /// session variables cleared. Runs the guard first.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        self.assert_isolated();
        let mut command = Command::new(program);
        crate::isolate(&mut command, &self.env);
        command
    }

    /// `niri msg <args>` against the nested niri, for example
    /// `["action", "switch-layout", "1"]`. Panics if it fails.
    pub fn niri_msg(&self, args: &[&str]) -> Output {
        assert_eq!(self.compositor, Compositor::NestedNiri, "niri_msg needs nested niri");
        let out = self.command(&tools().niri).arg("msg").args(args).output().expect("run niri msg");
        assert!(out.status.success(), "niri msg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out
    }

    /// The compositors' logs so far.
    pub fn logs(&self) -> String {
        let mut text = String::new();
        for name in ["sway.log", "niri.log"] {
            if let Ok(log) = fs::read_to_string(self.root.path().join("logs").join(name)) {
                text.push_str(&format!("--- {name}\n{log}"));
            }
        }
        text
    }

    /// Whether the compositors are still running.
    pub fn is_alive(&mut self) -> bool {
        [&mut self.host, &mut self.niri].into_iter().flatten().all(|c| matches!(c.try_wait(), Ok(None)))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for child in [&mut self.niri, &mut self.host].into_iter().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::env::var_os("PSST_SANDBOX_KEEP").is_some() {
            let dir = std::mem::replace(&mut self.root, tempfile::tempdir().expect("temp dir")).keep();
            eprintln!("sandbox: kept {}", dir.display());
        }
    }
}

/// A compositor command with an empty environment apart from the sandbox's directories, logging
/// to `logs/<log>`. It dies with the thread that started it.
fn compositor_command(program: &Path, dir: &Path, log: &str) -> Result<Command, String> {
    let log = fs::File::create(dir.join("logs").join(log)).map_err(|e| e.to_string())?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("HOME", dir.join("home"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LANG", "C.UTF-8")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
        .env("NO_AT_BRIDGE", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    // SAFETY: prctl is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
                .map_err(std::io::Error::from)
        });
    }
    Ok(command)
}

/// Waits for a socket named `<prefix>*` (not a `.lock`) in `dir`.
fn wait_for_socket(dir: &Path, prefix: &str, child: &mut Child, root: &Path, log: &str) -> Result<PathBuf, String> {
    let deadline = Instant::now() + STARTUP;
    loop {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_socket = entry.file_type().is_ok_and(|t| std::os::unix::fs::FileTypeExt::is_socket(&t));
                if name.starts_with(prefix) && !name.ends_with(".lock") && is_socket {
                    return Ok(entry.path());
                }
            }
        }
        let log_text = || fs::read_to_string(root.join("logs").join(log)).unwrap_or_default();
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the compositor exited with {status} before {prefix}* appeared; {log}:\n{}",
                log_text()
            ));
        }
        if Instant::now() > deadline {
            return Err(format!("no {prefix}* socket within {STARTUP:?}; {log}:\n{}", log_text()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Refuses a nested niri that loaded the NVIDIA driver or opened a GPU device.
fn no_gpu(pid: u32) -> Result<(), String> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
    if maps.contains("libnvidia") {
        return Err("the nested niri loaded the NVIDIA driver".into());
    }
    for fd in fs::read_dir(format!("/proc/{pid}/fd")).map_err(|e| e.to_string())?.flatten() {
        if let Ok(target) = fs::read_link(fd.path()) {
            let target = target.to_string_lossy();
            if target.starts_with("/dev/nvidia") || target.starts_with("/dev/dri/") {
                return Err(format!("the nested niri opened the GPU device {target}"));
            }
        }
    }
    Ok(())
}
