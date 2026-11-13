use std::{
    collections::{HashMap, HashSet},
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
use psst_proto::{
    DeviceId,
    api::ModelState,
    ws::{Feature, ServerLimits, ServerMsg, SettingsMsg},
};
use tokio::sync::{mpsc, watch};

use crate::{
    auth::{Identity, LoginLinks, Pairings, Refusal},
    db::Db,
    dictionary::Layered,
    engine::Supervisor,
    profiles::ProfileLayers,
    registry::{Attachment, ConnEvent, Registry},
    settings::ServerLayers,
};

#[derive(Debug)]
pub struct AppState {
    /// As the server started. Only the keys the web UI can't change stay so: the others are live
    /// in [`LiveConfig::server`].
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
    server: RwLock<Arc<ServerConfig>>,
    /// Held until the new settings are swapped in, so changes happen one at a time.
    server_layers: Mutex<ServerLayers>,
    /// Likewise for every device's settings.
    device_settings: Mutex<()>,
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
        server_layers: ServerLayers,
        (profile_layers, profiles): (ProfileLayers, ProfilesFile),
        (layered, dictionary): (Layered, Dictionary),
    ) -> Self {
        Self {
            dir,
            server: RwLock::new(Arc::new(server_layers.effective())),
            server_layers: Mutex::new(server_layers),
            device_settings: Mutex::new(()),
            profiles: RwLock::new(Arc::new(profiles)),
            profile_layers: Mutex::new(profile_layers),
            dictionary: RwLock::new(Arc::new(dictionary)),
            layered: Mutex::new(layered),
        }
    }

    /// server.toml with the web UI's changes.
    pub fn server(&self) -> Arc<ServerConfig> {
        self.server.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn server_layers(&self) -> MutexGuard<'_, ServerLayers> {
        self.server_layers.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_server(&self, server: ServerConfig) {
        *self.server.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(server);
    }

    pub fn device_settings(&self) -> MutexGuard<'_, ()> {
        self.device_settings.lock().unwrap_or_else(|e| e.into_inner())
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

    pub fn reload(&self) {
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
            Ok(current) if current != *self.server_layers().file() => {
                tracing::warn!("{} changed; restart psst-server to apply it", ServerConfig::FILE_NAME);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
pub struct ConnTable {
    next_id: AtomicU64,
    /// Token devices' connections: one each.
    by_device: Mutex<HashMap<DeviceId, Attachment>>,
    /// Every connection past `hello`, by id.
    joined: Mutex<HashMap<u64, Joined>>,
}

#[derive(Debug)]
struct Joined {
    device: DeviceId,
    tx: mpsc::UnboundedSender<ConnEvent>,
    accepts: Vec<Feature>,
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

    pub fn join(&self, conn: &Attachment, device: &DeviceId, accepts: &[Feature]) {
        let joined = Joined { device: device.clone(), tx: conn.tx.clone(), accepts: accepts.to_vec() };
        self.joined.lock().unwrap_or_else(|e| e.into_inner()).insert(conn.conn_id, joined);
    }

    pub fn leave(&self, conn_id: u64) {
        self.joined.lock().unwrap_or_else(|e| e.into_inner()).remove(&conn_id);
    }

    pub fn online(&self) -> HashSet<DeviceId> {
        self.joined.lock().unwrap_or_else(|e| e.into_inner()).values().map(|j| j.device.clone()).collect()
    }

    fn send(&self, feature: Feature, to: impl Fn(&DeviceId) -> bool, msg: ServerMsg) -> usize {
        let joined = self.joined.lock().unwrap_or_else(|e| e.into_inner());
        let targets = joined.values().filter(|j| to(&j.device) && j.accepts.contains(&feature));
        targets.filter(|j| j.tx.send(ConnEvent::Msg(msg.clone())).is_ok()).count()
    }

    /// To the device's connections that accept settings; how many there were.
    pub fn push_settings(&self, device: &DeviceId, overrides: &serde_json::Value) -> usize {
        let msg = ServerMsg::Settings(SettingsMsg { overrides: overrides.clone() });
        self.send(Feature::Settings, |d| d == device, msg)
    }

    pub fn push_limits(&self, limits: ServerLimits) -> usize {
        self.send(Feature::Limits, |_| true, ServerMsg::Limits(limits))
    }

    pub fn close(&self, device: &DeviceId, reason: &str) {
        if let Some(conn) = self.by_device.lock().unwrap_or_else(|e| e.into_inner()).remove(device) {
            let _ = conn.tx.send(ConnEvent::Close { reason: reason.into() });
        }
    }
}
