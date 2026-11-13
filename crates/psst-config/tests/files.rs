use std::{fs, path::PathBuf};

use psst_config::{
    AppRule, AudioInput, ClientConfig, Color, ConfigError, ConfigFile, DecodePath, DictionaryFile, PasteAction,
    PasteRule, ProfilesFile, Rule, ServerConfig, ServerFiles, TermLanguages, TermSpec, TermsAs,
};
use psst_proto::Endpoint;

#[track_caller]
fn invalid<T: ConfigFile + std::fmt::Debug>(text: &str, expected: &str) {
    let err = T::from_toml_str(text).unwrap_err();
    let message = err.to_string();
    assert!(message.starts_with(T::FILE_NAME), "error names its file: {message}");
    assert!(message.contains(expected), "{message:?} should mention {expected:?}");
}

#[test]
fn empty_files_mean_every_default() {
    assert_eq!(ServerConfig::from_toml_str("").unwrap(), ServerConfig::default());
    assert_eq!(ProfilesFile::from_toml_str("").unwrap(), ProfilesFile::default());
    assert_eq!(DictionaryFile::from_toml_str("").unwrap(), DictionaryFile::default());
    assert_eq!(ClientConfig::from_toml_str("").unwrap(), ClientConfig::default());
}

#[test]
fn defaults_round_trip_through_toml() {
    assert_eq!(
        ServerConfig::from_toml_str(&ServerConfig::default().to_toml_string()).unwrap(),
        ServerConfig::default()
    );
    assert_eq!(
        ProfilesFile::from_toml_str(&ProfilesFile::default().to_toml_string()).unwrap(),
        ProfilesFile::default()
    );
    assert_eq!(
        ClientConfig::from_toml_str(&ClientConfig::default().to_toml_string()).unwrap(),
        ClientConfig::default()
    );
}

#[test]
fn unknown_keys_are_errors() {
    invalid::<ServerConfig>("[model]\nmodle = \"x\"", "modle");
    invalid::<ProfilesFile>("[profiles.default]\nlanguge = \"en\"", "languge");
    invalid::<DictionaryFile>("[[terms]]\ntext = \"x\"\npinned = true", "pinned");
    invalid::<ClientConfig>("[audio]\ndevice = \"x\"", "device");
}

#[test]
fn server_model_section_as_in_the_plan() {
    let config = ServerConfig::from_toml_str(
        r#"
        load_deadline_s = 90
        listen = []

        [model]
        engine = "faster-whisper"
        command = ["psst-engine-faster-whisper", "--verbose"]
        model = "large-v3-turbo"
        revision = "abc123"
        device = "cuda"
        compute_type = "int8_float16"
        preload = true
        options = { path = "auto", beam_size = 5, word_timestamps = true, vad_filter = true, terms_as = "hotwords", batch_size = 8 }
        "#,
    )
    .unwrap();
    assert_eq!(config.load_deadline_s, 90);
    assert!(config.listen.is_empty());
    assert_eq!(config.model.revision.as_deref(), Some("abc123"));
    assert_eq!(config.model.options.path, DecodePath::Auto);
    assert_eq!(config.model.options.terms_as, TermsAs::Hotwords);
    assert_eq!(
        config.model.options.to_json(),
        serde_json::json!({"path": "auto", "beam_size": 5, "word_timestamps": true, "vad_filter": true,
                           "terms_as": "hotwords", "batch_size": 8})
    );
}

#[test]
fn server_rejects_settings_that_cannot_work() {
    invalid::<ServerConfig>("[model.options]\npath = \"batched\"\nvad_filter = false", "vad_filter");
    invalid::<ServerConfig>("[model.options]\npath = \"auto\"\nvad_filter = false", "vad_filter");
    invalid::<ServerConfig>("[model]\ncommand = []", "model.command");
    invalid::<ServerConfig>("[model.options]\nbeam_size = 0", "beam_size");
    invalid::<ServerConfig>("[limits]\nmax_active_sessions = 0", "limits");
    invalid::<ServerConfig>("listen = [\"desktop:7341\"]", "listen");
    invalid::<ServerConfig>("[model.options]\npath = \"parallel\"", "parallel");
}

const PLAN_PROFILES: &str = r#"
[profiles.default]
language = "en"
prompt_style = "I'm updating the NixOS config. Claude Code and t3code run fine."
rules = ["fillers", "whitespace", "capitalize", "trailing_space"]

[profiles.czech]
language = "cs"
prompt_style = "Upravuji konfiguraci NixOS a spouštím t3code."
rules = ["fillers", "whitespace", "capitalize", "trailing_space"]

[fillers]
en = ["um", "uh", "erm"]
cs = ["ehm", "eee"]
"#;

#[test]
fn profiles_resolve_with_their_languages_fillers() {
    let file = ProfilesFile::from_toml_str(PLAN_PROFILES).unwrap();
    let czech = file.resolve("czech").unwrap();
    assert_eq!(czech.name, "czech");
    assert_eq!(czech.language, "cs");
    assert_eq!(czech.prompt_style, "Upravuji konfiguraci NixOS a spouštím t3code.");
    assert_eq!(czech.rules, [Rule::Fillers, Rule::Whitespace, Rule::Capitalize, Rule::TrailingSpace]);
    assert_eq!(czech.fillers, ["ehm", "eee"]);
    assert_eq!(file.resolve("default").unwrap().fillers, ["um", "uh", "erm"]);
    assert!(file.resolve("missing").is_none());
    let names: Vec<_> = file.resolve_all().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["czech", "default"]);
}

#[test]
fn the_built_in_default_profile_is_english_with_every_rule() {
    let profile = ProfilesFile::default().resolve("default").unwrap();
    assert_eq!(profile.language, "en");
    assert_eq!(profile.prompt_style, "");
    assert_eq!(profile.fillers, ["um", "uh", "erm"]);
    assert_eq!(profile.rules.len(), 4);
}

#[test]
fn a_fillers_table_replaces_the_built_in_lists() {
    let file = ProfilesFile::from_toml_str("[profiles.default]\nlanguage = \"cs\"\n[fillers]\nen = [\"um\"]").unwrap();
    assert_eq!(file.resolve("default").unwrap().fillers, Vec::<String>::new());
}

#[test]
fn profiles_need_a_default_and_known_unique_rules() {
    invalid::<ProfilesFile>("[profiles.czech]\nlanguage = \"cs\"", "`default` profile");
    invalid::<ProfilesFile>("[profiles.default]\nrules = [\"fillers\", \"fillers\"]", "twice");
    invalid::<ProfilesFile>("[profiles.default]\nrules = [\"shout\"]", "shout");
    invalid::<ProfilesFile>("[profiles.default]\nlanguage = \"\"", "language");
}

#[test]
fn a_long_prompt_style_is_a_warning_not_an_error() {
    let long = "word ".repeat(100);
    let file = ProfilesFile::from_toml_str(&format!("[profiles.default]\nprompt_style = \"{long}\"")).unwrap();
    let warnings = file.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("profiles.default.prompt_style"), "{warnings:?}");
    assert!(ProfilesFile::from_toml_str(PLAN_PROFILES).unwrap().warnings().is_empty());
}

#[test]
fn dictionary_terms_with_defaults() {
    let file = DictionaryFile::from_toml_str(
        r#"
        [[terms]]
        text = "Hyprland"
        sounds_like = ["hyper land"]

        [[terms]]
        text = "Dvořák"
        languages = "cs"
        fuzzy = false
        pin = true
        notes = "composer"
        "#,
    )
    .unwrap();
    assert_eq!(file.terms[0], TermSpec { sounds_like: vec!["hyper land".into()], ..TermSpec::new("Hyprland") });
    let czech = &file.terms[1];
    assert_eq!(czech.languages, TermLanguages::Only("cs".into()));
    assert!(czech.languages.includes("cs") && !czech.languages.includes("en"));
    assert!(TermLanguages::Any.includes("en"));
    assert!(!czech.fuzzy && czech.pin);
    assert_eq!(czech.notes.as_deref(), Some("composer"));
}

#[test]
fn dictionary_terms_need_spellings() {
    invalid::<DictionaryFile>("[[terms]]\ntext = \" \"", "text");
    invalid::<DictionaryFile>("[[terms]]\ntext = \"x\"\nsounds_like = [\"\"]", "sounds_like");
    invalid::<DictionaryFile>("[[terms]]\ntext = \"x\"\nlanguages = \"\"", "languages");
    invalid::<DictionaryFile>("[[terms]]\nsounds_like = [\"x\"]", "text");
}

#[test]
fn client_test_seams_read_audio_from_a_file_without_sounds() {
    let config = ClientConfig::from_toml_str(
        r#"
        server = "http://desktop:7341"
        sounds = false
        [audio]
        input = "file:/tmp/psst-test/clip.wav"
        "#,
    )
    .unwrap();
    assert!(!config.sounds);
    assert_eq!(config.audio.input, AudioInput::File(PathBuf::from("/tmp/psst-test/clip.wav")));
    assert_eq!(config.server, Endpoint::Tcp { host: "desktop".into(), port: 7341 });
    assert_eq!(ClientConfig::from_toml_str(&config.to_toml_string()).unwrap(), config);
}

#[test]
fn client_audio_input_forms() {
    let input = |s: &str| ClientConfig::from_toml_str(&format!("[audio]\ninput = {s:?}")).map(|c| c.audio.input);
    assert_eq!(input("default").unwrap(), AudioInput::Default);
    assert_eq!(input("alsa_input.usb-mic").unwrap(), AudioInput::Device("alsa_input.usb-mic".into()));
    invalid::<ClientConfig>("[audio]\ninput = \"file:clip.wav\"", "absolute");
    invalid::<ClientConfig>("[audio]\ninput = \"\"", "audio.input");
}

#[test]
fn client_overlay_colours() {
    let config =
        ClientConfig::from_toml_str("[overlay]\nanchor = \"top\"\nbackground = \"#000000\"\naccent = \"#FF000080\"")
            .unwrap();
    assert_eq!(config.overlay.background, Color { r: 0, g: 0, b: 0, a: 255 });
    assert_eq!(config.overlay.accent, Color { r: 255, g: 0, b: 0, a: 128 });
    assert_eq!(config.overlay.accent.to_string(), "#ff000080");
    for bad in ["red", "#12345", "#1234567", "#gg0000", "#ééé"] {
        invalid::<ClientConfig>(&format!("[overlay]\nforeground = {bad:?}"), "colour");
    }
}

#[test]
fn paste_rules_by_app_id() {
    let config = ClientConfig::from_toml_str(
        r#"
        [apps."steam"]
        paste_delay_ms = 50

        [apps."org.keepassxc.KeePassXC"]
        paste = false

        [apps."com.mitchellh.ghostty"]
        paste = "ctrl+v"
        "#,
    )
    .unwrap();
    let rule = |id| config.paste_for(id);
    assert_eq!(rule(Some("steam")), PasteRule { action: PasteAction::CtrlV, delay_ms: 50 });
    assert_eq!(rule(Some("org.keepassxc.KeePassXC")).action, PasteAction::CopyOnly);
    assert_eq!(rule(Some("com.mitchellh.ghostty")).action, PasteAction::CtrlV, "a rule beats the terminal list");
    assert_eq!(rule(Some("foot")).action, PasteAction::CtrlShiftV);
    assert_eq!(rule(Some("helium")).action, PasteAction::CtrlV);
    assert_eq!(rule(None), PasteRule { action: PasteAction::CtrlV, delay_ms: 0 });

    assert_eq!(
        ClientConfig::from_toml_str(&config.to_toml_string()).unwrap().apps["org.keepassxc.KeePassXC"],
        AppRule { paste: Some(PasteAction::CopyOnly), paste_delay_ms: 0 }
    );
    invalid::<ClientConfig>("[apps.x]\npaste = true", "paste");
    invalid::<ClientConfig>("[apps.x]\npaste = \"ctrl+alt+v\"", "paste");
}

#[test]
fn server_files_load_from_a_directory_with_defaults_for_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(ServerFiles::load(dir.path()).unwrap(), ServerFiles::default());

    fs::write(dir.path().join("profiles.toml"), PLAN_PROFILES).unwrap();
    let files = ServerFiles::load(dir.path()).unwrap();
    assert!(files.profiles.resolve("czech").is_some());
    assert_eq!(files.server, ServerConfig::default());

    files.write(dir.path()).unwrap();
    assert_eq!(ServerFiles::load(dir.path()).unwrap(), files);
}

#[test]
fn errors_from_files_name_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    assert!(matches!(ServerConfig::load(&path), Err(ConfigError::Io { .. })));
    assert_eq!(ServerConfig::load_or_default(&path).unwrap(), ServerConfig::default());

    fs::write(&path, "eval_mode = 1").unwrap();
    let message = ServerFiles::load(dir.path()).unwrap_err().to_string();
    assert!(message.starts_with(&path.display().to_string()), "{message}");
}

#[test]
fn warnings_name_their_file() {
    let files = ServerFiles {
        profiles: ProfilesFile::from_toml_str(&format!("[profiles.default]\nprompt_style = \"{}\"", "x".repeat(500)))
            .unwrap(),
        ..ServerFiles::default()
    };
    let warnings: Vec<_> = files.warnings().iter().map(ToString::to_string).collect();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("profiles.toml: warning: "), "{warnings:?}");
}
