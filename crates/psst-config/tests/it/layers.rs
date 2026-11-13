use psst_config::{
    AppRule, ClientConfig, Color, ConfigFile, KeyPath, PasteAction, ServerConfig, Sounds,
    layers::{self, layer, sparse},
};
use serde_json::json;

fn client(toml: &str) -> ClientConfig {
    ClientConfig::from_toml_str(toml).unwrap()
}

#[test]
fn edits_override_the_file_key_by_key() {
    let file = client("sounds = \"glass\"\n[overlay]\nwidth = 300\nheight = 50\n");
    let layered = layer(&file, &json!({"overlay": {"width": 400}, "sounds": "off"}));
    assert!(layered.problems.is_empty(), "{:?}", layered.problems);
    assert_eq!(layered.config.overlay.width, 400);
    assert_eq!(layered.config.overlay.height, 50, "the file's other keys stay");
    assert_eq!(layered.config.sounds, Sounds::Off);
}

#[test]
fn a_bad_edit_falls_back_to_the_file_and_says_why() {
    let file = client("[overlay]\nwidth = 300\naccent = \"#112233\"\n");
    let edits = json!({"overlay": {"width": 0, "accent": "red", "margin": 20}, "colour": 1});
    let layered = layer(&file, &edits);
    assert_eq!(layered.config.overlay.width, 300);
    assert_eq!(layered.config.overlay.accent, "#112233".parse::<Color>().unwrap());
    assert_eq!(layered.config.overlay.margin, 20, "a good edit beside bad ones still applies");
    let problem = |key: &[&str]| layered.problem(&KeyPath::of(key)).unwrap_or_default().to_owned();
    assert!(problem(&["overlay", "width"]).contains("at least 1"), "{:?}", layered.problems);
    assert!(problem(&["overlay", "accent"]).contains("#rrggbb"), "{:?}", layered.problems);
    assert!(problem(&["colour"]).contains("unknown field"), "{:?}", layered.problems);
}

#[test]
fn only_the_file_sets_locked_keys() {
    let file = ClientConfig::default();
    let edits = json!({"server": "http://elsewhere:7341", "insert": {"record": "/tmp/x"}});
    let layered = layer(&file, &edits);
    assert_eq!(layered.config, file);
    assert_eq!(layered.problems.len(), 2, "{:?}", layered.problems);

    let server = layer(&ServerConfig::default(), &json!({"model": {"command": ["sh"], "model": "small"}}));
    assert_eq!(server.config.model.command, ServerConfig::default().model.command);
    assert_eq!(server.config.model.model, "small");
    assert!(server.problem(&KeyPath::parse("model.command")).unwrap().contains("server.toml"));
}

#[test]
fn null_removes_a_files_app_rule_or_optional_key() {
    let file = client(
        "[apps.\"org.gnome.Ptyxis\"]\npaste = \"ctrl+shift+v\"\npaste_delay_ms = 40\n[apps.kitty]\npaste = false\n",
    );
    let edits = json!({"apps": {"kitty": null, "org.gnome.Ptyxis": {"paste": null}, "foot": {"paste_delay_ms": 5}}});
    let layered = layer(&file, &edits);
    assert!(layered.problems.is_empty(), "{:?}", layered.problems);
    let apps = &layered.config.apps;
    assert!(!apps.contains_key("kitty"));
    assert_eq!(apps["org.gnome.Ptyxis"], AppRule { paste: None, paste_delay_ms: 40 });
    assert_eq!(apps["foot"], AppRule { paste: None, paste_delay_ms: 5 });
    assert_eq!(file.apps["kitty"].paste, Some(PasteAction::CopyOnly));
}

#[test]
fn sparse_keeps_what_differs_from_the_default_and_merges_back() {
    let file = client("profile = \"czech\"\n[overlay]\nwidth = 240\nheight = 60\n[apps.foot]\npaste_delay_ms = 0\n");
    let keys = sparse(&file);
    assert_eq!(keys, json!({"profile": "czech", "overlay": {"height": 60}, "apps": {"foot": {"paste_delay_ms": 0}}}));
    assert_eq!(layer(&ClientConfig::default(), &keys).config, file);

    let merged = layers::merge(&keys, &json!({"overlay": {"height": null, "width": 300}, "apps": {"foot": null}}));
    assert_eq!(merged, json!({"profile": "czech", "overlay": {"width": 300}, "apps": {}}));
}

#[test]
fn edits_are_set_and_removed_by_key() {
    let mut edits = json!({});
    let app = KeyPath::of(&["apps", "org.gnome.Ptyxis"]);
    layers::set_edit(&mut edits, &app, serde_json::Value::Null);
    layers::set_edit(&mut edits, &KeyPath::of(&["apps", "org.gnome.Ptyxis", "paste"]), json!("ctrl+v"));
    layers::set_edit(&mut edits, &KeyPath::parse("overlay.width"), json!(300));
    assert_eq!(edits, json!({"apps": {"org.gnome.Ptyxis": {"paste": "ctrl+v"}}, "overlay": {"width": 300}}));
    assert_eq!(app.to_string(), "apps.\"org.gnome.Ptyxis\"");

    assert!(layers::remove_edit(&mut edits, &app));
    assert!(!layers::remove_edit(&mut edits, &KeyPath::parse("overlay.height")));
    assert_eq!(edits, json!({"overlay": {"width": 300}}));
}
