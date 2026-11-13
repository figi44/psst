//! The JSON the Kotlin app sends and parses.

use serde_json::{Value, json};

use super::*;

#[test]
fn the_apps_commands_parse() {
    let id: SessionId = "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c".parse().unwrap();
    let commands = [
        (
            json!({"type": "configure", "server": "http://host:7341"}),
            Command::Configure { server: "http://host:7341".into() },
        ),
        (json!({"type": "pair", "code": "ABCD-1234"}), Command::Pair { code: "ABCD-1234".into() }),
        (json!({"type": "warm"}), Command::Warm),
        (json!({"type": "start"}), Command::Start),
        (json!({"type": "stop"}), Command::Stop),
        (json!({"type": "cancel"}), Command::Cancel),
        (json!({"type": "discard", "session_id": id.to_string()}), Command::Discard { session_id: id }),
        (json!({"type": "inserted", "session_id": id.to_string()}), Command::Inserted { session_id: id }),
        (json!({"type": "history"}), Command::History),
        (json!({"type": "start", "from": "a newer app"}), Command::Start),
    ];
    for (json, command) in commands {
        assert_eq!(serde_json::from_value::<Command>(json).unwrap(), command);
    }
    for malformed in [
        json!({"type": "explode"}),
        json!({"type": "pair"}),
        json!({"type": "inserted", "session_id": "x"}),
        json!({"type": "discard"}),
    ] {
        assert!(serde_json::from_value::<Command>(malformed.clone()).is_err(), "{malformed}");
    }
}

#[test]
fn events_have_the_contracts_shape() {
    let state = State {
        server: Some("http://host:7341".into()),
        server_version: Some("0.1.0".into()),
        link: Link::Online,
        link_message: None,
        dictation: Dictation::Idle,
        dictation_message: None,
        seconds_left: None,
        saved: vec![
            SavedDictation {
                session_id: "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c".parse().unwrap(),
                created_ms: 1_794_300_000_000,
                duration_ms: 34_000,
                text: Some("Hello world.".into()),
            },
            SavedDictation {
                session_id: "1c1f3b5d-9e4f-4a2b-8c7d-2e3f4a5b6c7d".parse().unwrap(),
                created_ms: 1_794_299_000_000,
                duration_ms: 2_500,
                text: None,
            },
        ],
    };
    assert_eq!(
        serde_json::to_value(Event::State { state }).unwrap(),
        json!({"type": "state", "state": {
            "server": "http://host:7341",
            "server_version": "0.1.0",
            "link": "online",
            "link_message": null,
            "dictation": "idle",
            "dictation_message": null,
            "seconds_left": null,
            "saved": [
                {
                    "session_id": "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c",
                    "created_ms": 1_794_300_000_000_i64,
                    "duration_ms": 34_000,
                    "text": "Hello world.",
                },
                {
                    "session_id": "1c1f3b5d-9e4f-4a2b-8c7d-2e3f4a5b6c7d",
                    "created_ms": 1_794_299_000_000_i64,
                    "duration_ms": 2_500,
                    "text": null,
                },
            ],
        }})
    );
    let id: SessionId = "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c".parse().unwrap();
    assert_eq!(
        serde_json::to_value(Event::Final { session_id: id, text: "Hello world.".into() }).unwrap(),
        json!({"type": "final", "session_id": "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c", "text": "Hello world."})
    );
    assert_eq!(
        serde_json::to_value(Event::Saved { session_id: id }).unwrap(),
        json!({"type": "saved", "session_id": "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c"})
    );
    let items = vec![HistoryItem { session_id: id, text: "Hello world.".into(), created_ms: 1_794_300_000_000 }];
    assert_eq!(
        serde_json::to_value(Event::History { items, error: None }).unwrap(),
        json!({"type": "history", "items": [{
            "session_id": "0b0e2a4c-8d3e-4f1a-9b6c-1d2e3f4a5b6c",
            "text": "Hello world.",
            "created_ms": 1_794_300_000_000_i64,
        }], "error": null})
    );
    assert_eq!(
        serde_json::to_value(Event::History { items: vec![], error: Some("Cannot reach the server.".into()) }).unwrap(),
        json!({"type": "history", "items": [], "error": "Cannot reach the server."})
    );

    let links = [
        (Link::Unconfigured, "unconfigured"),
        (Link::Connecting, "connecting"),
        (Link::Online, "online"),
        (Link::Offline, "offline"),
        (Link::NeedsPairing, "needs_pairing"),
        (Link::UpdateApp, "update_app"),
        (Link::UpdateServer, "update_server"),
    ];
    for (link, name) in links {
        assert_eq!(serde_json::to_value(link).unwrap(), Value::from(name));
    }
    let dictations = [
        (Dictation::Idle, "idle"),
        (Dictation::Recording, "recording"),
        (Dictation::Transcribing, "transcribing"),
        (Dictation::LoadingModel, "loading_model"),
        (Dictation::Sending, "sending"),
        (Dictation::WaitingForServer, "waiting_for_server"),
    ];
    for (dictation, name) in dictations {
        assert_eq!(serde_json::to_value(dictation).unwrap(), Value::from(name));
    }
}
