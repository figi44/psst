use psst_config::Anchor;
use psst_proto::ws::{Feature, ServerLimits};
use psst_wayland::{MenuAction, MenuOutcome};
use serde_json::json;

use super::*;

async fn until(d: &Daemon, what: &str, check: impl Fn(&Status) -> bool) -> Status {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = d.status().await;
        if check(&status) {
            return status;
        }
        assert!(tokio::time::Instant::now() < deadline, "never {what}: {status:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_servers_settings_go_over_client_toml_and_a_bad_one_keeps_the_files_value() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.set_settings(Some(json!({"profile": "notes", "sounds": "glass", "overlay": {"width": 0, "margin": 40}})));
    server.push_outcome(Outcome::Final("noted ".into()));
    let chimes: Arc<Mutex<Vec<(Sounds, Sound)>>> = Arc::default();
    let heard = chimes.clone();
    let d = daemon(&server, |cfg, deps| {
        cfg.overlay.height = 60;
        deps.sound = Arc::new(move |set, sound| heard.lock().unwrap().push((set, sound)));
    })
    .await;

    let hello = server.hellos().remove(0);
    assert_eq!(hello.accepts, [Feature::Settings, Feature::Limits]);
    let config = hello.config.expect("client.toml's keys");
    assert_eq!(config["overlay"], json!({"height": 60}), "only what the file sets: {config}");
    let status = until(&d, "on the server's profile", |s| s.profile == "notes").await;
    assert!(
        status.warnings.iter().any(|w| w.starts_with("server setting ignored: overlay.width")),
        "{:?}",
        status.warnings
    );

    let id = d.dictate(300).await;
    d.idle().await;
    assert_eq!(server.session(id).unwrap().starts[0].profile.as_deref(), Some("notes"));
    assert_eq!(*chimes.lock().unwrap(), [(Sounds::Glass, Sound::Start), (Sounds::Glass, Sound::Stop)]);
    wait_until(|| !d.wayland.configured.lock().unwrap().is_empty()).await;
    let overlay = d.wayland.configured.lock().unwrap()[0];
    assert_eq!((overlay.width, overlay.height, overlay.margin), (240, 60, 40), "the bad width stays the file's");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_during_a_dictation_waits_for_it_to_end() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.set_settings(Some(json!({})));
    server.push_outcome(Outcome::Final("first ".into()));
    server.push_outcome(Outcome::Final("second ".into()));
    let d = daemon(&server, |_, _| {}).await;

    let started = d.toggle().await;
    let first: SessionId = started.message.rsplit(' ').next().unwrap().parse().unwrap();
    until(&d, "recording", |s| s.state == "recording").await;
    server.set_settings(Some(json!({"profile": "notes", "overlay": {"anchor": "top"}})));
    until(&d, "on the new profile", |s| s.profile == "notes").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(d.wayland.configured.lock().unwrap().is_empty(), "the overlay showing keeps its place");
    assert!(d.toggle().await.ok);
    d.idle().await;
    assert_eq!(server.session(first).unwrap().starts[0].profile.as_deref(), Some("default"));

    wait_until(|| d.wayland.overlays().last() == Some(&None)).await;
    wait_until(|| !d.wayland.configured.lock().unwrap().is_empty()).await;
    assert_eq!(d.wayland.configured.lock().unwrap()[0].anchor, Anchor::Top);
    let second = d.dictate(300).await;
    d.idle().await;
    assert_eq!(server.session(second).unwrap().starts[0].profile.as_deref(), Some("notes"));
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_menus_profile_lasts_until_the_server_names_another() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.set_settings(Some(json!({})));
    let d = daemon(&server, |_, _| {}).await;
    assert!(d.run(Request::Menu).await.ok);
    wait_until(|| d.wayland.menus().last().is_some_and(Option::is_some)).await;
    d.wayland.send(WaylandEvent::Menu(MenuOutcome::Chose(MenuAction::SwitchProfile("cs".into()))));
    until(&d, "switched", |s| s.profile == "cs").await;

    server.set_settings(Some(json!({"sounds": "off"})));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(d.status().await.profile, "cs", "a change to something else keeps the menu's choice");
    server.set_settings(Some(json!({"sounds": "off", "profile": "work"})));
    until(&d, "on the server's profile", |s| s.profile == "work").await;
    server.set_settings(Some(json!({})));
    until(&d, "back on client.toml's", |s| s.profile == "default").await;
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_settings_hold_while_the_server_is_unreachable() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.set_settings(Some(json!({"profile": "notes"})));
    let dir = tempfile::tempdir().unwrap();
    let d = daemon_in(dir.path(), &server, |_, _| {}).await;
    until(&d, "on the server's profile", |s| s.profile == "notes").await;
    d.stop().await;

    server.set_listening(false);
    let offline = daemon_in(dir.path(), &server, |_, _| {}).await;
    assert_eq!(offline.status().await.profile, "notes");
    assert!(offline.toggle().await.ok);
    let status =
        until(&offline, "recording offline", |s| s.recording.as_ref().is_some_and(|r| r.offline.is_some())).await;
    assert_eq!(status.recording.unwrap().profile, "notes", "an offline dictation uses them too");
    offline.stop().await;

    // A server from before settings keeps none for this device.
    server.set_settings(None);
    server.set_listening(true);
    let connections = server.connections();
    let back = daemon_in(dir.path(), &server, |_, _| {}).await;
    wait_until(|| server.connections() > connections).await;
    until(&back, "back on client.toml's", |s| s.profile == "default").await;
    back.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_recording_limit_from_the_server_applies_to_the_next_dictation() {
    let server = FakeServer::start(FakeServerConfig::default());
    server.push_outcome(Outcome::Final("before ".into()));
    server.push_outcome(Outcome::Final("limited ".into()));
    let d = daemon(&server, |_, _| {}).await;
    server.push_limits(ServerLimits { max_recording_s: 1 });
    d.dictate(300).await;
    d.idle().await;

    let started = d.toggle().await;
    let id: SessionId = started.message.rsplit(' ').next().unwrap().parse().unwrap();
    d.idle().await;
    assert!(d.wayland.showed(|s| matches!(s, OverlayState::LimitWarning { .. })));
    let samples = server.session(id).unwrap().stop_samples.unwrap();
    assert!((16_000..24_000).contains(&samples), "stopped at the new limit: {samples}");
    d.stop().await;
}
