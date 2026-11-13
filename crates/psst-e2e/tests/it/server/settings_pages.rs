use psst_config::{ClientConfig, ConfigFile};
use psst_engine_proto::fake::Script;
use psst_proto::{
    api::{AppLayer, encode_segment},
    ws::Feature,
};
use psst_testkit::TestServer;
use serde_json::json;

use super::{
    access::pair,
    harness::*,
    pages::{logged_in, refused_with, see_other},
    settings::{api, connect_accepting, device_view, view},
};

fn web_server() -> TestServer {
    server(Script::default(), |cfg| cfg.tcp = true)
}

#[tokio::test]
async fn server_settings_change_and_reset_through_the_page() {
    let server = web_server();
    let browser = logged_in(&server).await;
    let api = api(&server);

    let page = browser.get("/settings").await;
    assert_eq!(page.status, 200);
    for text in ["Longest recording", "action=\"/settings/limits.max_recording_s\"", "configured via your Nix config"] {
        assert!(page.body.contains(text), "{text} on the page: {}", page.body);
    }
    assert!(!page.body.contains("action=\"/settings/listen\""), "a read-only setting has no form: {}", page.body);

    let saved = browser.post("/settings/limits.max_recording_s", &[("value", "120")]).await;
    see_other(&saved, "/settings?saved=limits.max_recording_s#s-limits-max_recording_s");
    let field = view(&api).await.field("limits.max_recording_s").unwrap().clone();
    assert_eq!((field.value, field.changed_here), (json!(120), true));
    let page = browser.get("/settings?saved=limits.max_recording_s").await;
    assert!(page.body.contains("Saved") && page.body.contains("Changed here"), "{}", page.body);
    assert!(page.body.contains("1 setting was changed here"), "{}", page.body);

    // An unticked switch sends no value, which is off.
    see_other(
        &browser.post("/settings/model.options.word_timestamps", &[]).await,
        "/settings?saved=model.options.word_timestamps#s-model-options-word_timestamps",
    );
    assert_eq!(view(&api).await.field("model.options.word_timestamps").unwrap().value, json!(false));

    let refused = browser.post("/settings/limits.max_recording_s", &[("value", "lots")]).await;
    refused_with(&refused, "Longest recording");
    assert!(refused.body.contains("value=\"lots\""), "what was typed stays: {}", refused.body);
    refused_with(&browser.post("/settings/listen", &[("value", "0.0.0.0:1")]).await, "Nix config");

    see_other(
        &browser.post("/settings/limits.max_recording_s/reset", &[]).await,
        "/settings?saved=limits.max_recording_s#s-limits-max_recording_s",
    );
    assert!(!view(&api).await.field("limits.max_recording_s").unwrap().changed_here);

    let export = browser.get("/settings.toml").await;
    assert_eq!(export.header("content-disposition"), Some("attachment; filename=\"server.toml\""));
    assert!(export.body.contains("word_timestamps = false"), "{}", export.body);
}

#[tokio::test]
async fn a_devices_settings_and_paste_rules_change_through_its_page() {
    let server = web_server();
    let browser = logged_in(&server).await;
    let api = api(&server);
    let file = ClientConfig::from_toml_str("sounds = \"glass\"\n[apps.kitty]\npaste = \"ctrl+shift+v\"\n").unwrap();
    let device = connect_accepting(&server, vec![Feature::Settings], Some(psst_config::layers::sparse(&file))).await;
    let id = device.welcome().device.clone();
    let base = format!("/devices/{}", encode_segment(&id));

    let list = browser.get("/devices").await;
    assert!(list.body.contains(&format!("href=\"{base}\"")) && list.body.contains("Connected"), "{}", list.body);
    let page = browser.get(&base).await;
    assert_eq!(page.status, 200);
    for text in ["psst 9.9.9", "Chimes", "<code>kitty</code>", "From its client.toml."] {
        assert!(page.body.contains(text), "{text} on the page: {}", page.body);
    }

    see_other(
        &browser.post(&format!("{base}/setting/sounds"), &[("value", "off")]).await,
        &format!("{base}?saved=sounds#s-sounds"),
    );
    assert_eq!(device_view(&api, &id).await.unwrap().settings.field("sounds").unwrap().value, json!("off"));

    let add = [("app", "org.gnome.Ptyxis"), ("paste", "ctrl+shift+v"), ("paste_delay_ms", "20")];
    see_other(&browser.post(&format!("{base}/app"), &add).await, &format!("{base}#apps"));
    refused_with(&browser.post(&format!("{base}/app"), &[("app", " "), ("paste", "auto")]).await, "Wayland app id");
    refused_with(
        &browser.post(&format!("{base}/app"), &[("app", "foot"), ("paste", "auto"), ("paste_delay_ms", "soon")]).await,
        "whole number of milliseconds",
    );
    see_other(&browser.post(&format!("{base}/app/kitty/remove"), &[]).await, &format!("{base}#apps"));
    let apps = device_view(&api, &id).await.unwrap().apps;
    assert_eq!(
        apps.iter().map(|a| (a.app.as_str(), a.layer)).collect::<Vec<_>>(),
        [("kitty", AppLayer::Hidden), ("org.gnome.Ptyxis", AppLayer::Added)]
    );

    let export = browser.get(&format!("{base}/client.toml")).await;
    assert_eq!(export.header("content-disposition"), Some("attachment; filename=\"client.toml\""));
    let exported = ClientConfig::from_toml_str(&export.body).unwrap_or_else(|err| panic!("{err}\n{}", export.body));
    assert_eq!(exported.apps.keys().collect::<Vec<_>>(), ["org.gnome.Ptyxis"]);

    let phone = pair(&server).await;
    let unshared = browser.get(&format!("/devices/{}", encode_segment(&phone.device_id))).await;
    assert!(unshared.body.contains("sent its settings"), "{}", unshared.body);
}
