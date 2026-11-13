//! The server's and each device's settings. Every row is its own form, so a change is one request
//! and a refusal names the row it belongs to.

use std::sync::Arc;

use axum::{
    Form,
    extract::{
        Path, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use jiff::{Timestamp, tz::TimeZone};
use minijinja::{Value, context};
use psst_proto::api::{
    AppEntry, AppLayer, AppRuleView, Applies, Device, DeviceKind, Setting, SettingAccess, SettingKind, SettingsView,
    encode_segment,
};
use serde::Deserialize;
use serde_json::Value as Json;

use super::{Viewer, chrome, dictionary::download, message, render};
use crate::{api::ApiErr, settings, state::AppState};

type AppRef = Arc<AppState>;

const PASTE: [(&str, &str); 4] =
    [("auto", "Automatic"), ("ctrl+v", "Ctrl+V"), ("ctrl+shift+v", "Ctrl+Shift+V"), ("copy", "Copy only")];

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    saved: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ValueForm {
    /// Absent for an unticked switch, which is off.
    #[serde(default)]
    value: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct AppForm {
    #[serde(default)]
    app: String,
    #[serde(default)]
    paste: String,
    #[serde(default)]
    paste_delay_ms: String,
}

/// A change the server turned down, shown on the row it was for, with what was typed kept.
#[derive(Debug, Default)]
struct Refusal {
    key: String,
    message: String,
    typed: Option<String>,
    status: Option<StatusCode>,
}

impl Refusal {
    fn of(key: &str, err: &ApiErr, typed: Option<String>) -> Refusal {
        let status = if err.status() == StatusCode::INTERNAL_SERVER_ERROR {
            err.status()
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        };
        Refusal { key: key.to_owned(), message: err.message().to_owned(), typed, status: Some(status) }
    }
}

/// A path built from [`encode_segment`]ed parts, which needs no escaping in an attribute.
fn path(path: String) -> Value {
    Value::from_safe_string(path)
}

fn shown(value: &Json) -> String {
    match value {
        Json::Null => String::new(),
        Json::String(text) => text.clone(),
        Json::Bool(true) => "On".into(),
        Json::Bool(false) => "Off".into(),
        Json::Array(items) => items.iter().map(shown).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

fn applies(applies: Applies) -> Option<&'static str> {
    match applies {
        Applies::Now => None,
        Applies::NextDictation => Some("From the next dictation."),
        Applies::NextConnect => Some("When the device next connects."),
        Applies::ModelReload => Some("The model reloads with it once no dictation is running."),
        Applies::NextStart => Some("When psst-server next starts."),
    }
}

/// `source` names where the file's values come from, as in "your Nix config has 30".
fn field(setting: &Setting, action: &str, source: &str, query: &PageQuery, refusal: Option<&Refusal>) -> Value {
    let mine = refusal.filter(|r| r.key == setting.key);
    let label_of = |value: &Json| match &setting.kind {
        SettingKind::Choice { options } => options
            .iter()
            .find(|option| value.as_str() == Some(option.value.as_str()))
            .map(|option| option.label.clone())
            .unwrap_or_else(|| shown(value)),
        SettingKind::Int { unit, .. } if !unit.is_empty() && !value.is_null() => format!("{} {unit}", shown(value)),
        _ => shown(value),
    };
    let value = mine.and_then(|r| r.typed.clone()).unwrap_or_else(|| shown(&setting.value));
    let (kind, min, max, unit, optional) = match &setting.kind {
        SettingKind::Bool => ("bool", 0, 0, "", false),
        SettingKind::Int { min, max, unit } => ("int", *min, *max, unit.as_str(), false),
        SettingKind::Choice { .. } => ("choice", 0, 0, "", false),
        SettingKind::Text { optional } => ("text", 0, 0, "", *optional),
        SettingKind::Color => ("color", 0, 0, "", false),
        SettingKind::List => ("list", 0, 0, "", false),
    };
    let options = match &setting.kind {
        SettingKind::Choice { options } => options
            .iter()
            .map(|option| {
                context! { value => option.value.clone(), label => option.label.clone(), selected => option.value == value }
            })
            .collect(),
        _ => Vec::new(),
    };
    context! {
        key => setting.key.clone(),
        id => format!("s-{}", setting.key.replace('.', "-")),
        action => path(format!("{action}/{}", encode_segment(&setting.key))),
        label => setting.label.clone(),
        help => setting.help.clone(),
        kind,
        min,
        max,
        unit,
        optional,
        options,
        value,
        checked => setting.value == Json::Bool(true),
        shown => label_of(&setting.value),
        changed => setting.changed_here && setting.problem.is_none(),
        file => setting.file.as_ref().map(label_of),
        default => label_of(&setting.default),
        source,
        readonly => matches!(setting.access, SettingAccess::ReadOnly { .. }),
        applies => applies(setting.applies),
        problem => setting.problem.clone(),
        error => mine.map(|r| r.message.clone()),
        saved => query.saved.as_deref() == Some(setting.key.as_str()),
    }
}

fn groups(view: &SettingsView, action: &str, source: &str, query: &PageQuery, refusal: Option<&Refusal>) -> Value {
    view.groups
        .iter()
        .map(|group| {
            let fields: Vec<Value> = group.fields.iter().map(|f| field(f, action, source, query, refusal)).collect();
            // Closed, the fold would hide the row a save or refusal just came back to.
            let open = group
                .fields
                .iter()
                .any(|f| query.saved.as_deref() == Some(f.key.as_str()) || refusal.is_some_and(|r| r.key == f.key));
            context! {
                id => group.id.clone(),
                title => group.title.clone(),
                advanced => group.advanced,
                open,
                fields,
            }
        })
        .collect()
}

fn saved(base: &str, key: &str) -> Response {
    Redirect::to(&format!("{base}?saved={}#s-{}", encode_segment(key), key.replace('.', "-"))).into_response()
}

fn bad_form(err: FormRejection) -> Response {
    message(StatusCode::BAD_REQUEST, "That form didn't come through", &err.body_text(), None)
}

fn server_page(state: &AppState, viewer: &Viewer, query: &PageQuery, refusal: Option<Refusal>) -> Response {
    let view = settings::server(state).view();
    let ctx = context! {
        groups => groups(&view, "/settings", "your Nix config", query, refusal.as_ref()),
        changes => view.changes,
        error => refusal.as_ref().filter(|r| view.field(&r.key).is_none()).map(|r| r.message.clone()),
        ..chrome(state, viewer, "settings")
    };
    render(refusal.and_then(|r| r.status).unwrap_or(StatusCode::OK), "settings.html", ctx)
}

pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    server_page(&state, &viewer, &query, None)
}

pub async fn set(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(key): Path<String>,
    form: Result<Form<ValueForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(err),
    };
    match settings::server(&state).set(&key, &form.value) {
        Ok(_) => saved("/settings", &key),
        Err(err) => {
            server_page(&state, &viewer, &PageQuery::default(), Some(Refusal::of(&key, &err, Some(form.value))))
        }
    }
}

pub async fn reset(viewer: Viewer, State(state): State<AppRef>, Path(key): Path<String>) -> Response {
    match settings::server(&state).reset(&key) {
        Ok(_) => saved("/settings", &key),
        Err(err) => server_page(&state, &viewer, &PageQuery::default(), Some(Refusal::of(&key, &err, None))),
    }
}

pub async fn export(_: Viewer, State(state): State<AppRef>) -> Response {
    download("server.toml", settings::server(&state).export())
}

/// "Connected", or when it was last seen: minutes, hours, then the date.
fn seen(device: &Device, now: Timestamp, tz: &TimeZone) -> String {
    if device.online {
        return "Connected".into();
    }
    let Some(at) = device.last_seen else { return "Never connected".into() };
    let minutes = (now.as_second() - at.as_second()) / 60;
    match minutes {
        ..1 => "Seen just now".into(),
        1..60 => format!("Seen {minutes} min ago"),
        60..1440 => format!("Seen {} h ago", minutes / 60),
        _ => format!("Seen {}", at.to_zoned(tz.clone()).strftime("%-d %B")),
    }
}

fn device_row(device: &Device, now: Timestamp, tz: &TimeZone) -> Value {
    let local = device.kind == DeviceKind::Local;
    context! {
        id => device.id.clone(),
        href => path(format!("/devices/{}", encode_segment(&device.id))),
        name => if local { device.name.trim_start_matches("local:").to_owned() } else { device.name.clone() },
        online => device.online,
        seen => seen(device, now, tz),
        how => match (local, device.reports_config) {
            (true, true) => "This machine · settings here",
            (true, false) => "This machine",
            (false, true) => "Paired · settings here",
            (false, false) => "Paired · settings on the device",
        },
    }
}

pub async fn devices(viewer: Viewer, State(state): State<AppRef>) -> Response {
    let list = match settings::devices(&state) {
        Ok(list) => list,
        Err(err) => return message(err.status(), "Devices are unavailable", err.message(), None),
    };
    let (now, tz) = (Timestamp::now(), TimeZone::system());
    let mut rows: Vec<&Device> = list.iter().collect();
    rows.sort_by_key(|d| (!d.online, std::cmp::Reverse(d.last_seen)));
    let ctx = context! {
        devices => rows.into_iter().map(|d| device_row(d, now, &tz)).collect::<Vec<_>>(),
        ..chrome(&state, &viewer, "devices")
    };
    render(StatusCode::OK, "devices.html", ctx)
}

fn app_row(entry: &AppEntry, base: &str, index: usize, refusal: Option<&Refusal>) -> Value {
    let mine = refusal.filter(|r| r.key == format!("app:{}", entry.app));
    let rule = entry
        .rule
        .clone()
        .or_else(|| entry.file.clone())
        .unwrap_or(AppRuleView { paste: "auto".into(), paste_delay_ms: 0 });
    let describe = |rule: &AppRuleView| {
        format!("{}, {} ms", PASTE.iter().find(|p| p.0 == rule.paste).map_or("?", |p| p.1), rule.paste_delay_ms)
    };
    let segment = encode_segment(&entry.app);
    context! {
        app => entry.app.clone(),
        id => format!("a-{index}"),
        layer => match entry.layer {
            AppLayer::File => "file",
            AppLayer::Replaced => "replaced",
            AppLayer::Added => "added",
            AppLayer::Hidden => "hidden",
        },
        paste => PASTE.map(|(value, label)| context! { value, label, selected => value == rule.paste }),
        delay => mine.and_then(|r| r.typed.clone()).unwrap_or_else(|| rule.paste_delay_ms.to_string()),
        file => entry.file.as_ref().map(describe),
        problem => entry.problem.clone(),
        error => mine.map(|r| r.message.clone()),
        action => path(format!("{base}/app")),
        remove => path(format!("{base}/app/{segment}/remove")),
        reset => path(format!("{base}/app/{segment}/reset")),
    }
}

fn device_page(state: &AppState, viewer: &Viewer, id: &str, query: &PageQuery, refusal: Option<Refusal>) -> Response {
    let list = match settings::devices(state) {
        Ok(list) => list,
        Err(err) => return message(err.status(), "Devices are unavailable", err.message(), None),
    };
    let Some(device) = list.iter().find(|d| d.id == id) else {
        return message(StatusCode::NOT_FOUND, "No such device", "It may have been unpaired.", None);
    };
    let (now, tz) = (Timestamp::now(), TimeZone::system());
    let base = format!("/devices/{}", encode_segment(id));
    let status = refusal.as_ref().and_then(|r| r.status).unwrap_or(StatusCode::OK);
    let view = settings::device(state, id).and_then(|settings| settings.view());
    let ctx = match view {
        Ok(view) => context! {
            device => device_row(device, now, &tz),
            client => view.client,
            base => path(base.clone()),
            groups => groups(&view.settings, &format!("{base}/setting"), "its client.toml", query, refusal.as_ref()),
            apps => view.apps.iter().enumerate().map(|(i, e)| app_row(e, &base, i, refusal.as_ref())).collect::<Vec<_>>(),
            paste => PASTE.map(|(value, label)| context! { value, label, selected => value == "auto" }),
            added => refusal.as_ref().filter(|r| r.key == "app:").map(|r| context! {
                error => r.message.clone(),
            }),
            changes => view.settings.changes,
            error => refusal.as_ref().filter(|r| !r.key.starts_with("app:") && view.settings.field(&r.key).is_none()).map(|r| r.message.clone()),
            ..chrome(state, viewer, "devices")
        },
        Err(err) if err.status() == StatusCode::NOT_FOUND => context! {
            device => device_row(device, now, &tz),
            unshared => err.message(),
            ..chrome(state, viewer, "devices")
        },
        Err(err) => return message(err.status(), "Settings are unavailable", err.message(), None),
    };
    render(status, "device.html", ctx)
}

pub async fn device(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    device_page(&state, &viewer, &id, &query, None)
}

pub async fn set_device(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path((id, key)): Path<(String, String)>,
    form: Result<Form<ValueForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(err),
    };
    match settings::device(&state, &id).and_then(|settings| settings.set(&key, &form.value)) {
        Ok(_) => saved(&format!("/devices/{}", encode_segment(&id)), &key),
        Err(err) => {
            let refusal = Refusal::of(&key, &err, Some(form.value));
            device_page(&state, &viewer, &id, &PageQuery::default(), Some(refusal))
        }
    }
}

pub async fn reset_device(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path((id, key)): Path<(String, String)>,
) -> Response {
    match settings::device(&state, &id).and_then(|settings| settings.reset(&key)) {
        Ok(_) => saved(&format!("/devices/{}", encode_segment(&id)), &key),
        Err(err) => device_page(&state, &viewer, &id, &PageQuery::default(), Some(Refusal::of(&key, &err, None))),
    }
}

fn apps_saved(id: &str) -> Response {
    Redirect::to(&format!("/devices/{}#apps", encode_segment(id))).into_response()
}

pub async fn set_app(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<AppForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(err),
    };
    let app = form.app.trim().to_owned();
    let refuse = |message: String| Refusal {
        key: format!("app:{app}"),
        message,
        typed: Some(form.paste_delay_ms.clone()),
        status: Some(StatusCode::UNPROCESSABLE_ENTITY),
    };
    let refusal = if app.is_empty() {
        Some(refuse("Type the app's Wayland app id, like org.mozilla.firefox.".into()))
    } else {
        match form.paste_delay_ms.trim() {
            "" => Ok(0),
            delay => delay.parse::<u32>().map_err(|_| refuse("Delay: a whole number of milliseconds.".into())),
        }
        .and_then(|paste_delay_ms| {
            let rule = AppRuleView { paste: form.paste.clone(), paste_delay_ms };
            settings::device(&state, &id)
                .and_then(|settings| settings.set_app(&app, &rule))
                .map_err(|err| Refusal::of(&format!("app:{app}"), &err, Some(form.paste_delay_ms.clone())))
        })
        .err()
    };
    match refusal {
        None => apps_saved(&id),
        Some(mut refusal) => {
            let known = settings::device(&state, &id)
                .and_then(|settings| settings.view())
                .is_ok_and(|view| view.apps.iter().any(|entry| entry.app == app));
            if !known {
                // A new rule's row doesn't exist yet: the error goes on the form that adds one.
                refusal.key = "app:".into();
            }
            device_page(&state, &viewer, &id, &PageQuery::default(), Some(refusal))
        }
    }
}

pub async fn remove_app(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path((id, app)): Path<(String, String)>,
) -> Response {
    match settings::device(&state, &id).and_then(|settings| settings.remove_app(&app)) {
        Ok(_) => apps_saved(&id),
        Err(err) => device_page(
            &state,
            &viewer,
            &id,
            &PageQuery::default(),
            Some(Refusal::of(&format!("app:{app}"), &err, None)),
        ),
    }
}

pub async fn reset_app(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path((id, app)): Path<(String, String)>,
) -> Response {
    match settings::device(&state, &id).and_then(|settings| settings.reset_app(&app)) {
        Ok(_) => apps_saved(&id),
        Err(err) => device_page(
            &state,
            &viewer,
            &id,
            &PageQuery::default(),
            Some(Refusal::of(&format!("app:{app}"), &err, None)),
        ),
    }
}

pub async fn export_device(_: Viewer, State(state): State<AppRef>, Path(id): Path<String>) -> Response {
    match settings::device(&state, &id).and_then(|settings| settings.export()) {
        Ok(toml) => download("client.toml", toml),
        Err(err) => message(err.status(), "No settings to download", err.message(), None),
    }
}
