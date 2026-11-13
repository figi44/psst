use std::time::Duration;

use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_config::{ClientConfig, ConfigFile, ServerConfig};
use psst_proto::{
    ErrorCode, SessionId,
    api::{
        ApiErrorCode, AppEntry, AppLayer, AppRuleView, Applies, Device, DeviceSettingsView, HistoryRow, ModelState,
        ModelStatus, Setting, SettingAccess, SettingsExport, SettingsView, routes,
    },
    ws::{ClientInfo, Feature, HelloMsg, Mode, ServerLimits, ServerMsg, SettingsMsg},
};
use psst_testkit::TestServer;
use serde_json::{Value, json};

use super::harness::*;

pub(super) fn api(server: &TestServer) -> ApiClient {
    ApiClient::new(server.endpoint(), None)
}

pub(super) async fn view(api: &ApiClient) -> SettingsView {
    api.call(Method::Get, routes::SETTINGS, None::<&()>).await.expect("GET /api/settings")
}

async fn field(api: &ApiClient, key: &str) -> Setting {
    view(api).await.field(key).unwrap_or_else(|| panic!("no setting {key}")).clone()
}

async fn set(api: &ApiClient, key: &str, value: &str) -> Result<Setting, ApiCallError> {
    api.call(Method::Put, &routes::with(routes::SETTING, "key", key), Some(&json!({"value": value}))).await
}

async fn reset(api: &ApiClient, key: &str) -> Result<Setting, ApiCallError> {
    api.call(Method::Post, &routes::with(routes::SETTING_RESET, "key", key), None::<&()>).await
}

#[track_caller]
fn refused<T: std::fmt::Debug>(result: Result<T, ApiCallError>) -> (ApiErrorCode, String) {
    match result {
        Err(ApiCallError::Api { error, .. }) => (error.code, error.message),
        other => panic!("expected an API error, got {other:?}"),
    }
}

fn file_server(adjust: impl FnOnce(&mut ServerConfig)) -> TestServer {
    server(script(vec![]), |cfg| adjust(&mut cfg.server))
}

#[tokio::test]
async fn server_settings_change_over_the_file_and_reset_to_it() {
    let mut server = file_server(|cfg| cfg.limits.max_recording_s = 300);
    let api = api(&server);
    let max = field(&api, "limits.max_recording_s").await;
    assert_eq!((max.value.clone(), max.file.clone(), max.default.clone()), (json!(300), Some(json!(300)), json!(600)));
    assert!(!max.changed_here);
    assert_eq!((max.access, max.applies), (SettingAccess::Editable, Applies::NextDictation));

    let changed = set(&api, "limits.max_recording_s", " 120 ").await.unwrap();
    assert_eq!((changed.value, changed.changed_here), (json!(120), true));
    set(&api, "model.options.terms_as", "hotwords").await.unwrap();
    set(&api, "history_days", "30").await.expect("the default, which the file has");
    assert!(!field(&api, "history_days").await.changed_here, "a value that changes nothing isn't kept");
    assert_eq!(view(&api).await.changes, 2);

    let export: SettingsExport = api.call(Method::Get, routes::SETTINGS_EXPORT, None::<&()>).await.unwrap();
    assert!(
        export.toml.contains("# Changed in the web UI: limits.max_recording_s, model.options.terms_as."),
        "{}",
        export.toml
    );
    let exported = ServerConfig::from_toml_str(&export.toml).unwrap_or_else(|err| panic!("{err}\n{}", export.toml));
    assert_eq!(
        (exported.limits.max_recording_s, exported.model.options.terms_as),
        (120, psst_config::TermsAs::Hotwords)
    );
    assert!(exported.listen.is_empty(), "the file's own keys stay: {}", export.toml);

    server.restart();
    let api = self::api(&server);
    assert_eq!(field(&api, "limits.max_recording_s").await.value, json!(120), "changes outlive a restart");
    assert_eq!(Client::connect(&server).await.welcome().limits.max_recording_s, 120);

    let back = reset(&api, "limits.max_recording_s").await.unwrap();
    assert_eq!((back.value, back.changed_here), (json!(300), false));
    let (code, message) = refused(reset(&api, "limits.max_recording_s").await);
    assert_eq!(code, ApiErrorCode::NotFound, "{message}");
}

#[tokio::test]
async fn the_server_refuses_read_only_settings_and_values_its_files_couldnt_have() {
    let server = default_server();
    let api = api(&server);
    let command = field(&api, "model.command").await;
    let SettingAccess::ReadOnly { reason } = &command.access else { panic!("{command:?}") };
    assert!(reason.contains("any program"), "{reason}");

    for (key, value) in [
        ("model.command", "/bin/sh\n-c\nrm -rf ~"),
        ("local_users", "mallory"),
        ("auto_pair", "evil-laptop"),
        ("listen", "0.0.0.0:1"),
        ("socket", "/tmp/elsewhere.sock"),
        ("eval_mode", "on"),
        ("model.engine", "other"),
        ("tailscale_socket", "/tmp/fake-tailscaled.sock"),
    ] {
        let (code, message) = refused(set(&api, key, value).await);
        assert_eq!(code, ApiErrorCode::Forbidden, "{key}: {message}");
        assert!(message.contains("Nix config"), "{key}: {message}");
    }
    let (code, message) = refused(set(&api, "limits.max_recording_s", "a minute").await);
    assert_eq!(code, ApiErrorCode::BadRequest);
    assert_eq!(message, "Longest recording: enter a whole number from 1 to 3600 s");
    let (_, message) = refused(set(&api, "model.options.vad_filter", "off").await);
    assert!(message.starts_with("Skip silence: "), "{message}");
    assert!(message.contains("vad_filter must be true"), "the file's rules hold: {message}");
    let (_, message) = refused(set(&api, "web_url", "desktop:7341").await);
    assert!(message.contains("http://host"), "{message}");
    wait_model(&server, ModelState::Loaded).await;
    let (_, message) = refused(set(&api, "limits.max_recording_s", "901").await);
    assert!(message.contains("at most 900 s"), "the engine's own limit: {message}");
    let (code, _) = refused(set(&api, "no.such.key", "1").await);
    assert_eq!(code, ApiErrorCode::NotFound);
    assert_eq!(view(&api).await.changes, 0);
}

#[tokio::test]
async fn a_file_over_the_engines_limit_still_lets_other_settings_change() {
    let server = file_server(|cfg| cfg.limits.max_recording_s = 1000);
    let api = api(&server);
    wait_model(&server, ModelState::Failed).await;
    set(&api, "history_days", "10").await.expect("unrelated to the limit");
    let (_, message) = refused(set(&api, "limits.max_recording_s", "901").await);
    assert!(message.contains("at most 900 s"), "{message}");
    set(&api, "limits.max_recording_s", "900").await.expect("within the engine's limit");
}

pub(super) async fn connect_accepting(server: &TestServer, accepts: Vec<Feature>, config: Option<Value>) -> Client {
    let client = ClientInfo { name: "psst".into(), version: "9.9.9".into() };
    let hello = HelloMsg { protocol: psst_proto::PROTOCOL_VERSION, client, accepts, config };
    Socket::unix(server.socket()).await.hello_msg(hello).await.expect("welcome")
}

#[tokio::test]
async fn limits_apply_to_the_next_dictation_and_reach_connected_clients_that_take_them() {
    let server = server(script(vec![result("one", 0), result("two", 0)]), |_| {});
    let api = api(&server);
    let mut takes_limits = connect_accepting(&server, vec![Feature::Limits], None).await;
    let mut old = Client::connect(&server).await;

    // Recording before the change keeps its limit.
    let before = SessionId::new_v4();
    old.start(before, Mode::Upload, false).await;
    old.ready().await;
    set(&api, "limits.max_recording_s", "1").await.unwrap();
    assert_eq!(takes_limits.recv().await, ServerMsg::Limits(ServerLimits { max_recording_s: 1 }));
    old.audio(7 * SECOND).await;
    old.stop(before, 7 * SECOND as u64).await;
    assert_eq!(old.final_msg().await.r#final.text, "One ");

    assert_eq!(Client::connect(&server).await.welcome().limits.max_recording_s, 1);
    let after = SessionId::new_v4();
    old.start(after, Mode::Upload, false).await;
    old.ready().await;
    old.audio(7 * SECOND).await;
    assert_eq!(old.error().await.code, ErrorCode::TooLong);

    set(&api, "limits.max_active_sessions", "1").await.unwrap();
    let busy = SessionId::new_v4();
    let mut other = Client::connect(&server).await;
    old.start(SessionId::new_v4(), Mode::Upload, false).await;
    old.ready().await;
    other.start(busy, Mode::Upload, false).await;
    assert_eq!(other.error().await.code, ErrorCode::Busy);
}

#[tokio::test]
async fn keeping_less_history_deletes_older_dictations_at_once() {
    let server = default_server();
    let mut client = Client::connect(&server).await;
    client.upload(SessionId::new_v4(), SECOND / 4).await;
    client.final_msg().await;
    let db = rusqlite::Connection::open(server.state_dir().join("psst.db")).unwrap();
    let three_days_ago = jiff::Timestamp::now().as_millisecond() - 3 * 86_400_000;
    db.execute(
        "INSERT INTO history (device, session_id, profile, created_ms) VALUES ('local:old', ?1, 'default', ?2)",
        rusqlite::params![SessionId::new_v4().to_string(), three_days_ago],
    )
    .unwrap();
    let api = api(&server);
    let history = || async {
        let rows: Vec<HistoryRow> = api.call(Method::Get, routes::HISTORY, None::<&()>).await.unwrap();
        rows.into_iter().map(|row| row.device).collect::<Vec<_>>()
    };
    assert_eq!(history().await.len(), 2);

    set(&api, "history_days", "1").await.unwrap();
    assert_eq!(history().await, [client.welcome().device.clone()]);
}

async fn model(api: &ApiClient) -> ModelStatus {
    api.call(Method::Get, routes::MODEL, None::<&()>).await.unwrap()
}

fn loads(server: &TestServer) -> Vec<String> {
    requests_of(server, "load").iter().map(|r| r.header["model"].as_str().unwrap_or_default().to_owned()).collect()
}

#[tokio::test]
async fn a_new_model_loads_once_the_dictation_in_progress_finishes() {
    let server = server(script(vec![result("kept", 0), result("after", 0)]), |cfg| cfg.tcp = true);
    let api = api(&server);
    wait_model(&server, ModelState::Loaded).await;
    let mut client = Client::connect(&server).await;
    let session = SessionId::new_v4();
    client.start(session, Mode::Upload, false).await;
    client.ready().await;

    let changed = set(&api, "model.model", "small.en").await.unwrap();
    assert_eq!(changed.applies, Applies::ModelReload);
    let status = model(&api).await;
    assert_eq!((status.state, status.name.as_str()), (ModelState::Loaded, "large-v3-turbo"));
    let reload = status.reload.expect("a reload waiting");
    assert_eq!((reload.model.as_str(), reload.waiting_for), ("small.en", 1));
    let described = String::from_utf8(admin(&server, &["model", "status"]).stdout).unwrap();
    assert!(described.contains("switching to small.en once the dictation in progress finishes"), "{described}");
    let page = super::pages::logged_in(&server).await.get("/history").await;
    assert!(
        page.body.contains("Switching to small.en</strong> once the dictation in progress finishes."),
        "{}",
        page.body
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(loads(&server), ["large-v3-turbo"], "nothing reloads mid-dictation");

    client.audio(SECOND / 2).await;
    client.stop(session, SECOND as u64 / 2).await;
    assert_eq!(client.final_msg().await.r#final.text, "Kept ", "the old model finishes it");
    let deadline = tokio::time::Instant::now() + MESSAGE_TIMEOUT;
    while loads(&server).len() < 2 || model(&api).await.state != ModelState::Loaded {
        assert!(tokio::time::Instant::now() < deadline, "no reload: {:?}", loads(&server));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(loads(&server), ["large-v3-turbo", "small.en"]);
    let status = model(&api).await;
    assert_eq!((status.name.as_str(), status.reload), ("small.en", None));
    let next = SessionId::new_v4();
    client.upload(next, SECOND / 4).await;
    assert_eq!(client.final_msg().await.r#final.text, "After ");
}

pub(super) async fn device_view(api: &ApiClient, id: &str) -> Result<DeviceSettingsView, ApiCallError> {
    api.call(Method::Get, &routes::with_id(routes::DEVICE_SETTINGS, id), None::<&()>).await
}

async fn set_device(api: &ApiClient, id: &str, key: &str, value: &str) -> Result<Setting, ApiCallError> {
    let path = routes::with(&routes::with_id(routes::DEVICE_SETTING, id), "key", key);
    api.call(Method::Put, &path, Some(&json!({"value": value}))).await
}

fn app_path(template: &str, id: &str, app: &str) -> String {
    routes::with(&routes::with_id(template, id), "app", app)
}

async fn settings_msg(client: &mut Client) -> Value {
    loop {
        match client.recv().await {
            ServerMsg::Settings(SettingsMsg { overrides }) => return overrides,
            ServerMsg::Progress(_) => {}
            other => panic!("expected settings, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn device_settings_reach_the_device_when_it_connects_and_at_once_while_it_is() {
    let server = default_server();
    let api = api(&server);
    let file = ClientConfig::from_toml_str(
        "server = \"http://gpu-box:7341\"\nsounds = \"glass\"\n[overlay]\nwidth = 320\n[apps.kitty]\npaste = false\n[apps.\"org.gnome.Ptyxis\"]\npaste_delay_ms = 20\n",
    )
    .unwrap();
    let config = psst_config::layers::sparse(&file);
    let mut device = connect_accepting(&server, vec![Feature::Settings], Some(config.clone())).await;
    assert_eq!(device.welcome().settings, Some(json!({})));
    let id = device.welcome().device.clone();
    let mut old = Client::connect(&server).await;
    assert_eq!(old.welcome().settings, None);

    let devices: Vec<Device> = api.call(Method::Get, routes::DEVICES, None::<&()>).await.unwrap();
    let listed = devices.iter().find(|d| d.id == id).expect("listed");
    assert!(listed.online && listed.reports_config, "{listed:?}");
    let view = device_view(&api, &id).await.unwrap();
    assert_eq!(view.client, "psst 9.9.9");
    let sounds = view.settings.field("sounds").unwrap();
    assert_eq!((&sounds.value, &sounds.file), (&json!("glass"), &Some(json!("glass"))));
    let server_key = view.settings.field("server").unwrap();
    assert!(matches!(server_key.access, SettingAccess::ReadOnly { .. }), "{server_key:?}");
    assert_eq!(server_key.value, json!("http://gpu-box:7341"), "the device's own file sets it, though edits can't");
    assert_eq!(
        view.apps.iter().map(|a| (a.app.as_str(), a.layer)).collect::<Vec<_>>(),
        [("kitty", AppLayer::File), ("org.gnome.Ptyxis", AppLayer::File)]
    );

    let changed = set_device(&api, &id, "sounds", "off").await.unwrap();
    assert_eq!((changed.value, changed.changed_here), (json!("off"), true));
    assert_eq!(settings_msg(&mut device).await, json!({"sounds": "off"}));
    set_device(&api, &id, "overlay.accent", "#00ff0080").await.unwrap();
    assert_eq!(settings_msg(&mut device).await, json!({"sounds": "off", "overlay": {"accent": "#00ff0080"}}));

    let rule = AppRuleView { paste: "ctrl+shift+v".into(), paste_delay_ms: 30 };
    let added: AppEntry = api.call(Method::Put, &app_path(routes::DEVICE_APP, &id, "foot"), Some(&rule)).await.unwrap();
    assert_eq!((added.layer, added.rule), (AppLayer::Added, Some(rule)));
    let hidden: Option<AppEntry> =
        api.call(Method::Delete, &app_path(routes::DEVICE_APP, &id, "kitty"), None::<&()>).await.unwrap();
    assert_eq!(hidden.map(|a| a.layer), Some(AppLayer::Hidden));
    let pushed = loop {
        let overrides = settings_msg(&mut device).await;
        if overrides["apps"].get("kitty").is_some() {
            break overrides;
        }
    };
    assert_eq!(pushed["apps"], json!({"foot": {"paste": "ctrl+shift+v", "paste_delay_ms": 30}, "kitty": null}));
    assert!(old.try_recv(Duration::from_millis(300)).await.is_none(), "an older client gets no settings");

    let again = connect_accepting(&server, vec![Feature::Settings], Some(config)).await;
    assert_eq!(again.welcome().settings, Some(pushed));

    let export: SettingsExport =
        api.call(Method::Get, &routes::with_id(routes::DEVICE_SETTINGS_EXPORT, &id), None::<&()>).await.unwrap();
    let exported = ClientConfig::from_toml_str(&export.toml).unwrap_or_else(|err| panic!("{err}\n{}", export.toml));
    assert_eq!(exported.sounds, psst_config::Sounds::Off);
    assert_eq!(exported.server, file.server, "the download still reaches the server");
    assert_eq!(exported.overlay.width, 320);
    assert_eq!(exported.apps.keys().collect::<Vec<_>>(), ["foot", "org.gnome.Ptyxis"]);

    let (code, message) = refused(set_device(&api, &id, "server", "http://elsewhere:7341").await);
    assert_eq!(code, ApiErrorCode::Forbidden, "{message}");
    let (code, message) = refused(set_device(&api, &id, "overlay.width", "0").await);
    assert_eq!((code, message.as_str()), (ApiErrorCode::BadRequest, "Width: enter a whole number from 1 to 2000 px"));
}

#[tokio::test]
async fn devices_that_never_sent_their_config_have_no_settings_to_edit() {
    let server = default_server();
    let old = Client::connect(&server).await;
    let (code, message) = refused(device_view(&api(&server), &old.welcome().device).await);
    assert_eq!(code, ApiErrorCode::NotFound);
    assert!(message.contains("hasn't sent its settings"), "{message}");
}
