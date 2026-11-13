use std::{
    fs, io,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use psst_proto::{DeviceId, Token};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Config {
    /// As the user gave it, normalised when it parses.
    pub server: Option<String>,
    pub device_id: Option<DeviceId>,
    pub token: Option<Token>,
}

impl Config {
    pub fn load(dir: &Path) -> Config {
        let path = path(dir);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                warn!("ignoring {}: {err}", path.display());
                Config::default()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(err) => {
                warn!("cannot read {}: {err}", path.display());
                Config::default()
            }
        }
    }

    /// Owner-only, since it holds the token.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let path = path(dir);
        let tmp = path.with_extension("json.tmp");
        // `mode` applies only to a new file, so not to one a crash left behind.
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}
