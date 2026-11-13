//! Server and device settings as the web UI changes them: each file's values with the changes
//! layered over them key by key ([`psst_config::layers`]), the changes stored in the database.
//!
//! The server's apply as they're made. A device's go to its connections that accept them, and
//! wait in the database for the others; the device layers them over its own client.toml, which it
//! sends when it connects.

mod schema;

use std::{collections::BTreeSet, path::PathBuf};

use psst_config::{
    AppRule, ClientConfig, Color, KeyPath, Layerable, Layered, PasteAction, Problem, ServerConfig,
    layers::{self, edit, get, layer, leaves, remove_edit, set_edit, sparse, to_json},
};
use psst_proto::{
    api::{
        ApiErrorCode, AppEntry, AppLayer, AppRuleView, Device, DeviceSettingsView, Setting, SettingAccess, SettingKind,
        SettingOption, SettingsGroup, SettingsView,
    },
    ws::ServerLimits,
};
use serde_json::{Value, json};

use self::schema::{Group, Kind, Spec};
use crate::{
    api::ApiErr,
    db::{self, DeviceConfig},
    engine::EngineConfig,
    state::AppState,
};

#[derive(Debug)]
struct Layers<T> {
    file: T,
    /// The keys the file sets other than to their defaults.
    file_keys: Value,
    overrides: Value,
}

impl<T: Layerable> Layers<T> {
    fn new(file: T, overrides: Value) -> Self {
        Layers { file_keys: sparse(&file), file, overrides }
    }

    fn layered(&self) -> Layered<T> {
        layer(&self.file, &self.overrides)
    }

    /// The overrides with `key` set to `value`, or removed for `None`, unless that leaves a key
    /// out that wasn't before. A value that changes nothing over the file isn't kept.
    fn propose(&self, key: &KeyPath, value: Option<Value>) -> Result<Value, Problem> {
        let before = self.layered();
        let mut without = self.overrides.clone();
        remove_edit(&mut without, key);
        let Some(value) = value else { return Ok(without) };
        let mut with = without.clone();
        set_edit(&mut with, key, value);
        let after = layer(&self.file, &with);
        let new_problem = after.problems.iter().find(|p| p.key.starts_with(key) || before.problem(&p.key).is_none());
        if let Some(problem) = new_problem {
            return Err(problem.clone());
        }
        Ok(if after.config == layer(&self.file, &without).config { without } else { with })
    }

    /// Only the changes that apply.
    fn applied(&self, layered: &Layered<T>) -> Value {
        let mut out = json!({});
        for (key, value) in leaves(&self.overrides) {
            if layered.problem(&key).is_none() {
                set_edit(&mut out, &key, value);
            }
        }
        out
    }

    fn export(&self, intro: &str) -> String {
        let applied = self.applied(&self.layered());
        let mut out = format!(
            "# {} with the changes made in psst's web UI, exported by psst-server {}.\n",
            T::FILE_NAME,
            env!("CARGO_PKG_VERSION")
        );
        out.push_str(intro);
        let changed: Vec<String> = leaves(&applied).into_iter().map(|(key, _)| key.to_string()).collect();
        if !changed.is_empty() {
            out.push_str(&format!("# Changed in the web UI: {}.\n", changed.join(", ")));
        }
        out.push('\n');
        out.push_str(&toml::to_string(&layers::merge(&self.file_keys, &applied)).expect("settings serialise as TOML"));
        out
    }
}

/// server.toml and the web UI's changes to it.
#[derive(Debug)]
pub struct ServerLayers {
    layers: Layers<ServerConfig>,
    cache_dir: PathBuf,
}

impl ServerLayers {
    pub fn load(file: ServerConfig, db: &db::Db, cache_dir: PathBuf) -> anyhow::Result<ServerLayers> {
        let layers = Layers::new(file, db.settings(db::SERVER_SCOPE)?);
        for problem in layers.layered().problems {
            tracing::error!("settings: leaving out the web UI's {}: {}", problem.key, problem.message);
        }
        Ok(ServerLayers { layers, cache_dir })
    }

    pub fn effective(&self) -> ServerConfig {
        self.layers.layered().config
    }

    pub fn file(&self) -> &ServerConfig {
        &self.layers.file
    }

    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig::from_server(&self.effective(), self.cache_dir.clone())
    }
}

/// Named by the setting's label when it's the one being changed.
fn refused(spec: &Spec, problem: Problem) -> ApiErr {
    if problem.key == KeyPath::parse(spec.key) {
        bad_request(format!("{}: {}", spec.label, problem.message))
    } else {
        bad_request(format!("{}: {}", problem.key, problem.message))
    }
}

fn bad_request(message: impl Into<String>) -> ApiErr {
    ApiErr::new(ApiErrorCode::BadRequest, message)
}

fn not_found(message: impl Into<String>) -> ApiErr {
    ApiErr::new(ApiErrorCode::NotFound, message)
}

fn spec<'a>(groups: &'a [Group], key: &str) -> Result<&'a Spec, ApiErr> {
    let spec = groups
        .iter()
        .flat_map(|group| group.fields)
        .find(|spec| spec.key == key)
        .ok_or_else(|| not_found(format!("no setting `{key}`")))?;
    match spec.read_only {
        Some(reason) => {
            Err(ApiErr::new(ApiErrorCode::Forbidden, format!("{} can't be changed here. {reason}", spec.label)))
        }
        None => Ok(spec),
    }
}

/// What a setting's kind needs from the server: the names a choice offers.
struct Options<'a> {
    state: &'a AppState,
}

impl Options<'_> {
    fn kind(&self, spec: &Spec, values: &[&Value]) -> SettingKind {
        let option = |value: &str, label: &str| SettingOption { value: value.into(), label: label.into() };
        let mut options: Vec<SettingOption> = match &spec.kind {
            Kind::Bool => return SettingKind::Bool,
            Kind::Int { min, max, unit } => return SettingKind::Int { min: *min, max: *max, unit: (*unit).into() },
            Kind::Text { optional } => return SettingKind::Text { optional: *optional },
            Kind::Color => return SettingKind::Color,
            Kind::List => return SettingKind::List,
            Kind::Choice(options) => options.iter().map(|(value, label)| option(value, label)).collect(),
            Kind::Profiles => {
                let profiles = self.state.live.profile_layers().effective();
                profiles.profiles.keys().map(|name| option(name, name)).collect()
            }
            Kind::PostProcessing => std::iter::once(option("none", "Off"))
                .chain(crate::tidy::entries(self.state).iter().map(|entry| option(&entry.id, &entry.name)))
                .collect(),
        };
        // The file may name one the list doesn't have, such as a profile only another server has.
        for value in values.iter().filter_map(|value| value.as_str()) {
            if !options.iter().any(|o| o.value == value) {
                options.push(option(value, value));
            }
        }
        SettingKind::Choice { options }
    }
}

/// A choice shows as its value even where the file writes another type: `sounds = false`.
fn shown(spec: &Spec, value: &Value) -> Value {
    match (&spec.kind, value) {
        (Kind::Choice(_) | Kind::Profiles | Kind::PostProcessing, Value::Bool(false)) => json!("off"),
        _ => value.clone(),
    }
}

fn parse(spec: &Spec, kind: &SettingKind, form: &str) -> Result<Value, ApiErr> {
    let refuse = |what: String| Err(bad_request(format!("{}: {what}", spec.label)));
    let text = form.trim();
    match kind {
        SettingKind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => Ok(json!(true)),
            "false" | "off" | "no" | "0" | "" => Ok(json!(false)),
            _ => refuse("choose on or off".into()),
        },
        SettingKind::Int { min, max, unit } => match text.parse::<u64>() {
            Ok(n) if (*min..=*max).contains(&n) => Ok(json!(n)),
            _ => refuse(format!("enter a whole number from {min} to {max}{}", unit_suffix(unit))),
        },
        SettingKind::Choice { options } => match options.iter().find(|o| o.value == text) {
            Some(option) => Ok(json!(option.value)),
            None => {
                let names: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
                refuse(format!("choose one of {}", names.join(", ")))
            }
        },
        SettingKind::Text { optional: true } if text.is_empty() => Ok(Value::Null),
        SettingKind::Text { .. } if text.is_empty() => refuse("it can't be empty".into()),
        SettingKind::Text { .. } => Ok(json!(text)),
        SettingKind::Color => match text.parse::<Color>() {
            Ok(color) => Ok(json!(color.to_string())),
            Err(_) => refuse("enter a colour as #rrggbb, or #rrggbbaa with transparency".into()),
        },
        SettingKind::List => Ok(json!(
            text.lines().flat_map(|line| line.split(',')).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()
        )),
    }
}

fn unit_suffix(unit: &str) -> String {
    match unit {
        "" => String::new(),
        "days" => " days".into(),
        unit => format!(" {unit}"),
    }
}

fn view<T: Layerable>(layers: &Layers<T>, groups: &[Group], options: &Options<'_>) -> SettingsView {
    let layered = layers.layered();
    let effective = to_json(&layered.config);
    let defaults = to_json(&T::default());
    let field = |spec: &Spec| {
        let key = KeyPath::parse(spec.key);
        let value = get(&effective, &key).map(|v| shown(spec, v)).unwrap_or(Value::Null);
        let file = get(&layers.file_keys, &key).map(|v| shown(spec, v));
        let default = get(&defaults, &key).map(|v| shown(spec, v)).unwrap_or(Value::Null);
        Setting {
            key: spec.key.into(),
            label: spec.label.into(),
            help: spec.help.into(),
            kind: options.kind(spec, &[&value, file.as_ref().unwrap_or(&Value::Null), &default]),
            changed_here: edit(&layers.overrides, &key).is_some(),
            problem: layered.problem(&key).map(String::from),
            access: match spec.read_only {
                Some(reason) => SettingAccess::ReadOnly { reason: reason.into() },
                None => SettingAccess::Editable,
            },
            applies: spec.applies,
            value,
            file,
            default,
        }
    };
    let groups: Vec<SettingsGroup> = groups
        .iter()
        .map(|group| SettingsGroup {
            id: group.id.into(),
            title: group.title.into(),
            advanced: group.advanced,
            fields: group.fields.iter().map(field).collect(),
        })
        .collect();
    let changes = groups.iter().flat_map(|g| &g.fields).filter(|f| f.changed_here && f.problem.is_none()).count();
    SettingsView { groups, changes }
}

fn setting(view: SettingsView, key: &str) -> Setting {
    view.groups.into_iter().flat_map(|g| g.fields).find(|f| f.key == key).expect("the key is in the schema")
}

/// The server's settings: server.toml with the web UI's changes.
pub fn server(state: &AppState) -> ServerSettings<'_> {
    ServerSettings { state }
}

pub struct ServerSettings<'a> {
    state: &'a AppState,
}

impl ServerSettings<'_> {
    pub fn view(&self) -> SettingsView {
        view(&self.state.live.server_layers().layers, schema::SERVER, &Options { state: self.state })
    }

    /// `form_value` as a form field has it: `on`, a number, an option's value, text.
    pub fn set(&self, key: &str, form_value: &str) -> Result<Setting, ApiErr> {
        let spec = spec(schema::SERVER, key)?;
        let mut layers = self.state.live.server_layers();
        let current = view(&layers.layers, schema::SERVER, &Options { state: self.state });
        let kind = &current.field(key).expect("the key is in the schema").kind;
        let value = parse(spec, kind, form_value)?;
        let overrides = layers.layers.propose(&KeyPath::parse(key), Some(value)).map_err(|p| refused(spec, p))?;
        let proposed = layer(&layers.layers.file, &overrides).config;
        if let Some(max) = self.state.engine.max_audio_s()
            && proposed.limits.max_recording_s != layers.effective().limits.max_recording_s
            && proposed.limits.max_recording_s > max
        {
            return Err(bad_request(format!("{}: the engine takes recordings of at most {max} s", spec.label)));
        }
        self.store(&mut layers, overrides)?;
        Ok(setting(view(&layers.layers, schema::SERVER, &Options { state: self.state }), key))
    }

    /// Back to server.toml's value.
    pub fn reset(&self, key: &str) -> Result<Setting, ApiErr> {
        let spec = spec(schema::SERVER, key)?;
        let mut layers = self.state.live.server_layers();
        let path = KeyPath::parse(key);
        if edit(&layers.layers.overrides, &path).is_none() {
            return Err(not_found(format!("{key} wasn't changed here")));
        }
        let overrides = layers.layers.propose(&path, None).map_err(|p| refused(spec, p))?;
        self.store(&mut layers, overrides)?;
        Ok(setting(view(&layers.layers, schema::SERVER, &Options { state: self.state }), key))
    }

    /// server.toml as it would be with the web UI's changes: only the keys either sets.
    pub fn export(&self) -> String {
        self.state.live.server_layers().layers.export(
            "# Use it as the NixOS module's settings.server (builtins.fromTOML reads it); once the\n\
             # server runs with it, reset each setting changed in the web UI.\n",
        )
    }

    fn store(&self, layers: &mut ServerLayers, overrides: Value) -> Result<(), ApiErr> {
        self.state.db.put_settings(db::SERVER_SCOPE, &overrides)?;
        let old = self.state.live.server();
        layers.layers.overrides = overrides;
        let new = layers.effective();
        // Published first, so a client connecting while the limits go out gets the new ones.
        self.state.live.set_server(new.clone());
        apply_server(self.state, &old, &new, layers);
        Ok(())
    }
}

fn apply_server(state: &AppState, old: &ServerConfig, new: &ServerConfig, layers: &ServerLayers) {
    let (before, after) = (to_json(old), to_json(new));
    for (key, value) in leaves(&layers.layers.overrides) {
        if get(&before, &key) != get(&after, &key) {
            tracing::info!("settings: {key} is now {value} in the web UI");
        }
    }
    if old.limits.max_active_sessions != new.limits.max_active_sessions {
        state.registry.set_max_active(new.limits.max_active_sessions as usize);
    }
    if (&old.model, old.limits.max_recording_s, old.load_deadline_s)
        != (&new.model, new.limits.max_recording_s, new.load_deadline_s)
    {
        state.engine.reconfigure(layers.engine_config());
    }
    if old.limits.max_recording_s != new.limits.max_recording_s {
        let told = state.conns.push_limits(ServerLimits { max_recording_s: new.limits.max_recording_s });
        tracing::info!("limits.max_recording_s is now {} s; told {told} connections", new.limits.max_recording_s);
    }
    if old.history_days != new.history_days {
        match state.db.prune_history(new.history_days) {
            Ok(0) => {}
            Ok(pruned) => tracing::info!("pruned {pruned} history rows older than {} days", new.history_days),
            Err(err) => tracing::warn!("cannot prune history: {err:#}"),
        }
    }
}

/// Paired and local devices, with which are connected.
pub fn devices(state: &AppState) -> Result<Vec<Device>, ApiErr> {
    let online = state.conns.online();
    let mut devices = state.db.devices()?;
    for device in &mut devices {
        device.online = online.contains(&device.id);
    }
    Ok(devices)
}

/// A device's settings: its client.toml, as it last sent it, with the web UI's changes. Only for
/// devices that sent one.
pub fn device<'a>(state: &'a AppState, id: &str) -> Result<DeviceSettings<'a>, ApiErr> {
    let device =
        devices(state)?.into_iter().find(|d| d.id == id).ok_or_else(|| not_found(format!("no device {id}")))?;
    let reported = state.db.device_config(id)?.ok_or_else(|| {
        not_found(format!(
            "{} hasn't sent its settings: psst on a desktop sends them when it connects, from version {} on",
            device.name,
            env!("CARGO_PKG_VERSION")
        ))
    })?;
    Ok(DeviceSettings { state, device, reported })
}

pub struct DeviceSettings<'a> {
    state: &'a AppState,
    device: Device,
    reported: DeviceConfig,
}

impl DeviceSettings<'_> {
    fn layers(&self) -> Result<Layers<ClientConfig>, ApiErr> {
        // The device checks its own file; one from a newer client may have keys this server
        // doesn't know, which it leaves out.
        let file = layers::read(&ClientConfig::default(), &self.reported.config).config;
        Ok(Layers::new(file, self.state.db.settings(&db::device_scope(&self.device.id))?))
    }

    fn settings_view(&self, layers: &Layers<ClientConfig>) -> SettingsView {
        let mut view = view(layers, schema::CLIENT, &Options { state: self.state });
        view.changes += apps(layers).iter().filter(|app| app.layer != AppLayer::File && app.problem.is_none()).count();
        view
    }

    pub fn view(&self) -> Result<DeviceSettingsView, ApiErr> {
        let layers = self.layers()?;
        Ok(DeviceSettingsView {
            device: self.device.clone(),
            client: self.reported.client.clone(),
            reported_at: self.reported.reported_at,
            settings: self.settings_view(&layers),
            apps: apps(&layers),
        })
    }

    pub fn set(&self, key: &str, form_value: &str) -> Result<Setting, ApiErr> {
        let spec = spec(schema::CLIENT, key)?;
        let _changing = self.state.live.device_settings();
        let layers = self.layers()?;
        let current = view(&layers, schema::CLIENT, &Options { state: self.state });
        let value = parse(spec, &current.field(key).expect("the key is in the schema").kind, form_value)?;
        let overrides = layers.propose(&KeyPath::parse(key), Some(value)).map_err(|p| refused(spec, p))?;
        let layers = self.store(layers, overrides)?;
        Ok(setting(view(&layers, schema::CLIENT, &Options { state: self.state }), key))
    }

    /// Back to the device's client.toml.
    pub fn reset(&self, key: &str) -> Result<Setting, ApiErr> {
        let spec = spec(schema::CLIENT, key)?;
        let _changing = self.state.live.device_settings();
        let layers = self.layers()?;
        let path = KeyPath::parse(key);
        if edit(&layers.overrides, &path).is_none() {
            return Err(not_found(format!("{key} wasn't changed here")));
        }
        let overrides = layers.propose(&path, None).map_err(|p| refused(spec, p))?;
        let layers = self.store(layers, overrides)?;
        Ok(setting(view(&layers, schema::CLIENT, &Options { state: self.state }), key))
    }

    /// Adds a paste rule, or replaces the one there is.
    pub fn set_app(&self, app: &str, rule: &AppRuleView) -> Result<AppEntry, ApiErr> {
        if app.is_empty() || app.chars().any(char::is_control) {
            return Err(bad_request("the app needs its Wayland app id, such as org.gnome.Ptyxis"));
        }
        let paste = match rule.paste.as_str() {
            "auto" => Value::Null,
            "ctrl+v" | "ctrl+shift+v" => json!(rule.paste),
            "copy" => json!(false),
            other => {
                return Err(bad_request(format!("paste `{other}`: choose auto, ctrl+v, ctrl+shift+v or copy")));
            }
        };
        let _changing = self.state.live.device_settings();
        let layers = self.layers()?;
        let base = KeyPath::of(&["apps", app]);
        let mut without = layers.overrides.clone();
        remove_edit(&mut without, &base);
        let mut with = without.clone();
        set_edit(&mut with, &KeyPath::of(&["apps", app, "paste"]), paste);
        set_edit(&mut with, &KeyPath::of(&["apps", app, "paste_delay_ms"]), json!(rule.paste_delay_ms));
        let before = layers.layered();
        let after = layer(&layers.file, &with);
        if let Some(problem) =
            after.problems.iter().find(|p| p.key.starts_with(&base) || before.problem(&p.key).is_none())
        {
            return Err(bad_request(format!("{}: {}", problem.key, problem.message)));
        }
        let overrides = if after.config == layer(&layers.file, &without).config { without } else { with };
        let layers = self.store(layers, overrides)?;
        Ok(apps(&layers).into_iter().find(|entry| entry.app == app).expect("the rule was just set"))
    }

    /// Hides the file's rule, or removes one added here.
    pub fn remove_app(&self, app: &str) -> Result<Option<AppEntry>, ApiErr> {
        let _changing = self.state.live.device_settings();
        let layers = self.layers()?;
        let base = KeyPath::of(&["apps", app]);
        let mut overrides = layers.overrides.clone();
        if layers.file.apps.contains_key(app) {
            set_edit(&mut overrides, &base, Value::Null);
        } else if !remove_edit(&mut overrides, &base) {
            return Err(not_found(format!("no paste rule for {app}")));
        }
        let layers = self.store(layers, overrides)?;
        Ok(apps(&layers).into_iter().find(|entry| entry.app == app))
    }

    /// Back to the file's rule, or none.
    pub fn reset_app(&self, app: &str) -> Result<Option<AppEntry>, ApiErr> {
        let _changing = self.state.live.device_settings();
        let layers = self.layers()?;
        let mut overrides = layers.overrides.clone();
        if !remove_edit(&mut overrides, &KeyPath::of(&["apps", app])) {
            return Err(not_found(format!("the paste rule for {app} wasn't changed here")));
        }
        let layers = self.store(layers, overrides)?;
        Ok(apps(&layers).into_iter().find(|entry| entry.app == app))
    }

    /// client.toml as it would be with the web UI's changes.
    pub fn export(&self) -> Result<String, ApiErr> {
        Ok(self.layers()?.export(&format!(
            "# Use it as {}'s client.toml, or the home-manager module's settings; once the device runs\n\
             # with it, reset each setting changed in the web UI.\n",
            self.device.name
        )))
    }

    fn store(&self, mut layers: Layers<ClientConfig>, overrides: Value) -> Result<Layers<ClientConfig>, ApiErr> {
        self.state.db.put_settings(&db::device_scope(&self.device.id), &overrides)?;
        let told = self.state.conns.push_settings(&self.device.id, &overrides);
        tracing::info!("settings of device {} changed; sent to {told} of its connections", self.device.id);
        layers.overrides = overrides;
        Ok(layers)
    }
}

fn rule_view(rule: &AppRule) -> AppRuleView {
    let paste = match rule.paste {
        None => "auto",
        Some(PasteAction::CtrlV) => "ctrl+v",
        Some(PasteAction::CtrlShiftV) => "ctrl+shift+v",
        Some(PasteAction::CopyOnly) => "copy",
    };
    AppRuleView { paste: paste.into(), paste_delay_ms: rule.paste_delay_ms }
}

fn apps(layers: &Layers<ClientConfig>) -> Vec<AppEntry> {
    let layered = layers.layered();
    let changed = layers.overrides.get("apps").and_then(Value::as_object);
    let ids: BTreeSet<&String> =
        layers.file.apps.keys().chain(changed.into_iter().flat_map(|apps| apps.keys())).collect();
    ids.into_iter()
        .map(|app| {
            let base = KeyPath::of(&["apps", app]);
            let file = layers.file.apps.get(app);
            let rule = layered.config.apps.get(app);
            let layer = match (file, changed.is_some_and(|apps| apps.contains_key(app)), rule) {
                (Some(_), false, _) => AppLayer::File,
                (Some(_), true, None) => AppLayer::Hidden,
                (Some(_), true, Some(_)) => AppLayer::Replaced,
                (None, _, _) => AppLayer::Added,
            };
            AppEntry {
                app: app.clone(),
                layer,
                rule: rule.map(rule_view),
                file: file.map(rule_view),
                problem: layered.problems.iter().find(|p| p.key.starts_with(&base)).map(|p| p.message.clone()),
            }
        })
        .collect()
}

/// What a device that accepts settings gets in `welcome`.
pub fn overrides(state: &AppState, device: &str) -> Value {
    state.db.settings(&db::device_scope(device)).unwrap_or_else(|err| {
        tracing::error!("cannot read the settings of device {device}: {err:#}");
        json!({})
    })
}

/// Stores the client.toml a device sent, as the base of its settings.
pub fn reported(state: &AppState, device: &str, config: &Value, client: &str) {
    let checked = layer(&ClientConfig::default(), config);
    for problem in &checked.problems {
        tracing::info!("device {device}'s client.toml: leaving out {}: {}", problem.key, problem.message);
    }
    if let Err(err) = state.db.put_device_config(device, config, client) {
        tracing::error!("cannot store the settings of device {device}: {err:#}");
    }
}
