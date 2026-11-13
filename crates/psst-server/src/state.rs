//! State shared by every connection and request.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use nix::unistd::{Uid, User};
use psst_config::{ConfigFile, DictionaryFile, Profile, ProfilesFile, ServerConfig};
use psst_dict::Dictionary;
use psst_proto::{DeviceId, api::ModelState};
use tokio::sync::watch;

use crate::{
    auth::{Identity, Pairings, Refusal},
    db::Db,
    engine::Supervisor,
    registry::{Attachment, ConnEvent, Registry},
};

/// Everything a connection or request handler needs.
#[derive(Debug)]
pub struct AppState {
    pub server: ServerConfig,
    pub live: LiveConfig,
    pub db: Db,
    pub registry: Arc<Registry>,
    pub engine: Supervisor,
    pub pairings: Pairings,
    pub conns: ConnTable,
    pub local: LocalPolicy,
    /// Becomes true when the server shuts down.
    pub shutdown: watch::Receiver<bool>,
}

impl AppState {
    pub fn model_loaded(&self) -> bool {
        self.engine.status().state == ModelState::Loaded
    }
}

/// Who may use the Unix socket.
#[derive(Debug, Clone)]
pub struct LocalPolicy {
    /// `local_users` from `server.toml`.
    pub users: Vec<String>,
    /// Also accept this user id: the server's own, when `local_users` is empty outside systemd.
    pub own_uid: Option<u32>,
}

impl LocalPolicy {
    /// The identity of a socket peer with this user id.
    pub fn identify(&self, uid: Option<u32>) -> Result<Identity, Refusal> {
        let uid = uid.ok_or(Refusal::Unauthorized)?;
        let name = User::from_uid(Uid::from_raw(uid)).ok().flatten().map(|user| user.name);
        if let Some(name) = &name
            && self.users.iter().any(|allowed| allowed == name)
        {
            return Ok(Identity::Local { user: name.clone() });
        }
        if self.own_uid == Some(uid) {
            return Ok(Identity::Local { user: name.unwrap_or_else(|| uid.to_string()) });
        }
        Err(Refusal::Forbidden)
    }
}

/// `profiles.toml` and `dictionary.toml`, replaced as a whole on reload. Sessions snapshot them at
/// `start`.
#[derive(Debug)]
pub struct LiveConfig {
    dir: PathBuf,
    profiles: RwLock<Arc<ProfilesFile>>,
    dictionary: RwLock<Arc<Dictionary>>,
}

impl LiveConfig {
    pub fn new(dir: PathBuf, profiles: ProfilesFile, dictionary: Dictionary) -> Self {
        Self { dir, profiles: RwLock::new(Arc::new(profiles)), dictionary: RwLock::new(Arc::new(dictionary)) }
    }

    pub fn profile(&self, name: &str) -> Option<Profile> {
        self.profiles.read().unwrap_or_else(|e| e.into_inner()).resolve(name)
    }

    pub fn dictionary(&self) -> Arc<Dictionary> {
        self.dictionary.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-reads both files. A file that fails validation keeps its old version, and the error is
    /// logged.
    pub fn reload(&self, server: &ServerConfig) {
        match ProfilesFile::load_or_default(&self.dir.join(ProfilesFile::FILE_NAME)) {
            Ok(profiles) => {
                *self.profiles.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(profiles);
                tracing::info!("{}: reloaded", ProfilesFile::FILE_NAME);
            }
            Err(err) => tracing::error!("{err}; keeping the previous {}", ProfilesFile::FILE_NAME),
        }
        let dictionary = DictionaryFile::load_or_default(&self.dir.join(DictionaryFile::FILE_NAME))
            .map_err(|err| err.to_string())
            .and_then(|file| {
                Dictionary::from_file(&file).map_err(|err| format!("{}: {err}", DictionaryFile::FILE_NAME))
            });
        match dictionary {
            Ok(dictionary) => {
                *self.dictionary.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(dictionary);
                tracing::info!("{}: reloaded", DictionaryFile::FILE_NAME);
            }
            Err(err) => tracing::error!("{err}; keeping the previous {}", DictionaryFile::FILE_NAME),
        }
        match ServerConfig::load_or_default(&self.dir.join(ServerConfig::FILE_NAME)) {
            Ok(current) if current != *server => {
                tracing::warn!("{} changed; restart psst-server to apply it", ServerConfig::FILE_NAME);
            }
            _ => {}
        }
    }
}

/// Live connections of paired devices, so a reconnect or a revoke closes the old one.
#[derive(Debug, Default)]
pub struct ConnTable {
    next_id: AtomicU64,
    by_device: Mutex<HashMap<DeviceId, Attachment>>,
}

impl ConnTable {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Registers a device's connection and closes its previous one.
    pub fn replace(&self, device: &DeviceId, conn: Attachment) {
        let old = self.by_device.lock().unwrap_or_else(|e| e.into_inner()).insert(device.clone(), conn);
        if let Some(old) = old {
            let _ = old.tx.send(ConnEvent::Close { reason: "replaced by a newer connection".into() });
        }
    }

    pub fn remove(&self, device: &DeviceId, conn_id: u64) {
        let mut conns = self.by_device.lock().unwrap_or_else(|e| e.into_inner());
        if conns.get(device).is_some_and(|conn| conn.conn_id == conn_id) {
            conns.remove(device);
        }
    }

    /// Closes a device's connection, if it has one.
    pub fn close(&self, device: &DeviceId, reason: &str) {
        if let Some(conn) = self.by_device.lock().unwrap_or_else(|e| e.into_inner()).remove(device) {
            let _ = conn.tx.send(ConnEvent::Close { reason: reason.into() });
        }
    }
}
