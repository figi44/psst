//! Finds the sandbox's programs: sway, niri, and mesa's EGL vendor file.
//!
//! In order: `PSST_SWAY`, `PSST_NIRI` and `PSST_MESA_EGL_VENDOR`; then `PATH` (and
//! `/run/opengl-driver` for mesa); then `nix build` from the flake's pinned nixpkgs (the path CI
//! takes). Paths from nix are cached in the target directory, keyed by `flake.lock`, so only the
//! first test process pays for the evaluation.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

#[derive(Debug, Clone)]
pub(crate) struct Tools {
    pub sway: PathBuf,
    pub niri: PathBuf,
    pub mesa_egl_vendor: PathBuf,
}

const MESA_VENDOR: &str = "share/glvnd/egl_vendor.d/50_mesa.json";

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the workspace exists")
}

/// The sandbox's tools, resolved once per process. Panics if one can't be found.
pub(crate) fn tools() -> &'static Tools {
    static TOOLS: OnceLock<Tools> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let mut missing = Vec::new();
        let sway = from_env("PSST_SWAY").or_else(|| on_path("sway"));
        let niri = from_env("PSST_NIRI").or_else(|| on_path("niri"));
        let mesa = from_env("PSST_MESA_EGL_VENDOR")
            .or_else(|| Some(Path::new("/run/opengl-driver").join(MESA_VENDOR)).filter(|p| p.exists()));
        if sway.is_none() {
            missing.push("sway");
        }
        if niri.is_none() {
            missing.push("niri");
        }
        if mesa.is_none() {
            missing.push("mesa");
        }
        let from_nix = if missing.is_empty() { Vec::new() } else { nix_paths(&missing) };
        let nix = |name: &str| from_nix.iter().find(|(n, _)| n == name).map(|(_, p)| p.clone());
        Tools {
            sway: sway.or_else(|| nix("sway").map(|p| p.join("bin/sway"))).expect("sway"),
            niri: niri.or_else(|| nix("niri").map(|p| p.join("bin/niri"))).expect("niri"),
            mesa_egl_vendor: mesa.or_else(|| nix("mesa").map(|p| p.join(MESA_VENDOR))).expect("mesa"),
        }
    })
}

fn from_env(name: &str) -> Option<PathBuf> {
    env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn on_path(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(program)).find(|p| p.is_file())
}

/// Store paths of nixpkgs packages from the flake's pinned nixpkgs, cached per `flake.lock`.
fn nix_paths(names: &[&str]) -> Vec<(String, PathBuf)> {
    let root = workspace_root();
    let lock = fs::read(root.join("flake.lock")).expect("read flake.lock");
    let cache = cache_dir().join(format!("nix-tools-{:016x}", fnv1a(&lock)));
    if let Ok(text) = fs::read_to_string(&cache) {
        let cached: Vec<(String, PathBuf)> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(name, path)| (name.to_owned(), PathBuf::from(path)))
            .collect();
        if names.iter().all(|n| cached.iter().any(|(c, p)| c == n && p.exists())) {
            return cached;
        }
    }
    let all = ["sway", "niri", "mesa"];
    let out = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths", "--inputs-from"])
        .arg(&root)
        .args(all.iter().map(|n| format!("nixpkgs#{n}")))
        .output()
        .unwrap_or_else(|e| panic!("sway, niri or mesa isn't on PATH and `nix` can't run: {e}"));
    assert!(out.status.success(), "nix build of sway, niri and mesa failed:\n{}", String::from_utf8_lossy(&out.stderr));
    let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout).lines().map(PathBuf::from).collect();
    assert_eq!(paths.len(), all.len(), "nix printed {paths:?}");
    let resolved: Vec<(String, PathBuf)> = all.iter().map(|n| n.to_string()).zip(paths).collect();
    let text: String = resolved.iter().map(|(n, p)| format!("{n}={}\n", p.display())).collect();
    let _ = fs::create_dir_all(cache_dir());
    let tmp = cache.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, &cache);
    }
    resolved
}

/// `<target>/psst-testkit`: next to the test binaries, so `cargo clean` clears it.
fn cache_dir() -> PathBuf {
    let target = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).or_else(|| {
        // <target>/<profile>/deps/<test binary>
        env::current_exe().ok()?.parent()?.parent()?.parent().map(Path::to_path_buf)
    });
    target.unwrap_or_else(env::temp_dir).join("psst-testkit")
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3))
}
