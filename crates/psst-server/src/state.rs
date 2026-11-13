use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use nix::unistd::{Uid, User};
use psst_config::{ConfigFile, DEFAULT_PROFILE, DictionaryFile, Profile, ProfilesFile, ServerConfig};
use psst_dict::Dictionary;
use psst_proto::{DeviceId, api::ModelState};
use tokio::sync::watch;

use crate::{
    auth::{Identity, LoginLinks, Pairings, Refusal},
    db::Db,
    dictionary::Layered,
    engine::Supervisor,
    profiles::ProfileLayers,
    registry::{Attachment, ConnEvent, Registry},
};

#[derive(Debug)]
pub struct AppState {
    pub server: ServerConfig,
    pub live: LiveConfig,
    pub db: Db,
    pub registry: Arc<Registry>,
    pub engine: Supervisor,
    pub pairings: Pairings,
    pub logins: LoginLinks,
    pub conns: ConnTable,
    pub local: LocalPolicy,
    pub tcp_addrs: Vec<SocketAddr>,
    pub shutdown: watch::Receiver<bool>,
    pub tidy: crate::tidy::Lab,
}

impl AppState {
    pub fn model_loaded(&self) -> bool {
        self.engine.status().state == ModelState::Loaded
    }
}

#[derive(Debug, Clone)]
pub struct LocalPolicy {
    pub users: Vec<String>,
    pub own_uid: Option<u32>,
}

impl LocalPolicy {
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

    pub fn allows_user(&self, user: &str) -> bool {
        if self.users.iter().any(|allowed| allowed == user) {
            return true;
        }
        let uid = User::from_name(user).ok().flatten().map(|user| user.uid.as_raw()).or_else(|| user.parse().ok());
        uid.is_some_and(|uid| self.own_uid == Some(uid))
    }
}

#[derive(Debug)]
pub struct LiveConfig {
    dir: PathBuf,
    profiles: RwLock<Arc<ProfilesFile>>,
    /// Held until the new profiles are swapped in, so changes and reloads happen one at a time.
    profile_layers: Mutex<ProfileLayers>,
    dictionary: RwLock<Arc<Dictionary>>,
    /// Held until the new dictionary is swapped in, so changes and reloads happen one at a time.
    layered: Mutex<Layered>,
}

impl LiveConfig {
    pub fn new(
        dir: PathBuf,
        profile_layers: ProfileLayers,
        profiles: ProfilesFile,
        layered: Layered,
        dictionary: Dictionary,
    ) -> Self {
        Self {
            dir,
            profiles: RwLock::new(Arc::new(profiles)),
            profile_layers: Mutex::new(profile_layers),
            dictionary: RwLock::new(Arc::new(dictionary)),
            layered: Mutex::new(layered),
        }
    }

    pub fn profile(&self, name: &str) -> Option<Profile> {
        self.profiles.read().unwrap_or_else(|e| e.into_inner()).resolve(name)
    }

    pub fn default_profile_in(&self, language: Option<&str>) -> Option<Profile> {
        let profiles = self.profiles.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut profile = profiles.resolve(DEFAULT_PROFILE)?;
        if let Some(language) = language {
            profile.language = language.to_owned();
            profile.fillers = profiles.fillers.get(language).cloned().unwrap_or_default();
        }
        Some(profile)
    }

    pub fn profile_layers(&self) -> MutexGuard<'_, ProfileLayers> {
        self.profile_layers.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_profiles(&self, profiles: ProfilesFile) {
        *self.profiles.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(profiles);
    }

    pub fn dictionary(&self) -> Arc<Dictionary> {
        self.dictionary.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn layered(&self) -> MutexGuard<'_, Layered> {
        self.layered.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_dictionary(&self, dictionary: Dictionary) {
        *self.dictionary.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(dictionary);
    }

    pub fn reload(&self, server: &ServerConfig) {
        let mut profile_layers = self.profile_layers();
        match ProfilesFile::load_or_default(&self.dir.join(ProfilesFile::FILE_NAME)) {
            Ok(file) => {
                self.set_profiles(profile_layers.reload(file));
                tracing::info!("{}: reloaded", ProfilesFile::FILE_NAME);
            }
            Err(err) => tracing::error!("{err}; keeping the previous {}", ProfilesFile::FILE_NAME),
        }
        drop(profile_layers);
        let mut layered = self.layered();
        let file =
            DictionaryFile::load_or_default(&self.dir.join(DictionaryFile::FILE_NAME)).map_err(|e| e.to_string());
        if let Some(dictionary) = layered.reload(file) {
            self.set_dictionary(dictionary);
        }
        drop(layered);
        match ServerConfig::load_or_default(&self.dir.join(ServerConfig::FILE_NAME)) {
            Ok(current) if current != *server => {
                tracing::warn!("{} changed; restart psst-server to apply it", ServerConfig::FILE_NAME);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
pub struct ConnTable {
    next_id: AtomicU64,
    by_device: Mutex<HashMap<DeviceId, Attachment>>,
}

impl ConnTable {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

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

    pub fn close(&self, device: &DeviceId, reason: &str) {
        if let Some(conn) = self.by_device.lock().unwrap_or_else(|e| e.into_inner()).remove(device) {
            let _ = conn.tx.send(ConnEvent::Close { reason: reason.into() });
        }
    }
}
