//! The JSON shapes of the wire types. Each test pins a message to the exact JSON it has on the
//! wire, in both directions.

use std::fmt::Debug;

use psst_proto::{
    ClientTimings, Edit, EngineResult, EngineTrace, ErrorCode, Final, Segment, ServerTimings, SessionId, Stage, Token,
    Word,
    api::{
        ApiError, ApiErrorCode, Device, DeviceKind, DownloadProgress, HistoryRow, ModelState, ModelStatus,
        PairRedeemed, Pairing, Percentiles, Stats, Term, routes,
    },
    ws::{
        AudioFormat, CancelMsg, CancelledMsg, ClientInfo, ClientMsg, ErrorMsg, EvalOpts, FinalMsg, HelloMsg, Mode,
        ProgressMsg, ProgressState, ReadyModelState, ReadyMsg, ReportMsg, ServerLimits, ServerMsg, StartMsg, StopMsg,
        WelcomeMsg,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// `value` serialises to exactly `wire`, and `wire` deserialises back to `value`.
#[track_caller]
fn assert_wire<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire, "serialised form");
    assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), value, "deserialised form");
}

const SID: &str = "6f1c0b6e-3c1a-4d5e-9f3b-2a7d8c9e0f11";

fn sid() -> SessionId {
    SID.parse().unwrap()
}

fn trace() -> EngineTrace {
    EngineTrace {
        engine: "faster-whisper".into(),
        model: "large-v3-turbo".into(),
        revision: "abc123".into(),
        path: "batched".into(),
        options: json!({"beam_size": 5}),
        prompt: "Hyprland, psst.".into(),
        result: EngineResult {
            language: "en".into(),
            speech_ms: Some(2100),
            segments: vec![Segment {
                text: " Push the fix.".into(),
                start_ms: 0,
                end_ms: 2100,
                no_speech_prob: Some(0.5),
                words: Some(vec![Word { text: " Push".into(), start_ms: 0, end_ms: 300, prob: 0.75 }]),
            }],
        },
    }
}

fn trace_json() -> Value {
    json!({
        "engine": "faster-whisper", "model": "large-v3-turbo", "revision": "abc123", "path": "batched",
        "options": {"beam_size": 5}, "prompt": "Hyprland, psst.",
        "result": {"language": "en", "speech_ms": 2100, "segments": [{
            "text": " Push the fix.", "start_ms": 0, "end_ms": 2100, "no_speech_prob": 0.5,
            "words": [{"text": " Push", "start_ms": 0, "end_ms": 300, "prob": 0.75}]
        }]}
    })
}

#[test]
fn hello_and_welcome() {
    assert_wire(
        &ClientMsg::Hello(HelloMsg {
            protocol: 1,
            client: ClientInfo { name: "psst".into(), version: "0.1.0".into() },
        }),
        json!({"type": "hello", "protocol": 1, "client": {"name": "psst", "version": "0.1.0"}}),
    );
    assert_wire(
        &ServerMsg::Welcome(WelcomeMsg {
            server_version: "0.1.0".into(),
            device: "local:alice".into(),
            limits: ServerLimits { max_recording_s: 600 },
        }),
        json!({"type": "welcome", "server_version": "0.1.0", "device": "local:alice",
               "limits": {"max_recording_s": 600}}),
    );
}

#[test]
fn start_with_defaults_omits_optional_fields() {
    assert_wire(
        &ClientMsg::Start(StartMsg {
            session_id: sid(),
            profile: None,
            audio: AudioFormat::default(),
            mode: Mode::Live,
            resume: false,
            eval: None,
        }),
        json!({"type": "start", "session_id": SID,
               "audio": {"codec": "pcm_s16le", "sample_rate": 16000, "frame_ms": 20}, "mode": "live"}),
    );
}

#[test]
fn start_with_every_field() {
    assert_wire(
        &ClientMsg::Start(StartMsg {
            session_id: sid(),
            profile: Some("czech".into()),
            audio: AudioFormat::default(),
            mode: Mode::Upload,
            resume: true,
            eval: Some(EvalOpts { bias: false }),
        }),
        json!({"type": "start", "session_id": SID, "profile": "czech",
               "audio": {"codec": "pcm_s16le", "sample_rate": 16000, "frame_ms": 20}, "mode": "upload",
               "resume": true, "eval": {"bias": false}}),
    );
}

#[test]
fn stop_cancel_and_report() {
    assert_wire(
        &ClientMsg::Stop(StopMsg { session_id: sid(), samples: 96_000 }),
        json!({"type": "stop", "session_id": SID, "samples": 96000}),
    );
    assert_wire(&ClientMsg::Cancel(CancelMsg { session_id: sid() }), json!({"type": "cancel", "session_id": SID}));
    assert_wire(
        &ClientMsg::Report(ReportMsg {
            session_id: sid(),
            history_id: 7,
            timings: ClientTimings { stop_to_dispatch_ms: Some(310), ..Default::default() },
        }),
        json!({"type": "report", "session_id": SID, "history_id": 7, "timings": {"stop_to_dispatch_ms": 310}}),
    );
}

#[test]
fn ready_and_progress() {
    assert_wire(
        &ServerMsg::Ready(ReadyMsg {
            session_id: sid(),
            model_state: ReadyModelState::Loading,
            resumed: false,
            need_audio: true,
        }),
        json!({"type": "ready", "session_id": SID, "model_state": "loading", "resumed": false, "need_audio": true}),
    );
    assert_wire(
        &ServerMsg::Progress(ProgressMsg { session_id: sid(), state: ProgressState::Retrying, received_ms: 1500 }),
        json!({"type": "progress", "session_id": SID, "state": "retrying", "received_ms": 1500}),
    );
}

#[test]
fn final_carries_result_timings_and_optional_trace() {
    let msg = ServerMsg::Final(Box::new(FinalMsg {
        session_id: sid(),
        history_id: 7,
        r#final: Final {
            text: "Push the fix. ".into(),
            raw_text: "Push the fix.".into(),
            edits: vec![Edit {
                stage: Stage::Rules,
                rule: "trailing_space".into(),
                term: None,
                raw_span: 13..13,
                before: "".into(),
                after: " ".into(),
                score: None,
            }],
            language: "en".into(),
        },
        timings: ServerTimings {
            audio_ms: 2100,
            queue_ms: 1,
            load_wait_ms: 0,
            engine_ms: 180,
            attempts: 1,
            pipeline_ms: 2,
            stop_to_final_ms: 190,
        },
        trace: Some(trace()),
    }));
    assert_wire(
        &msg,
        json!({"type": "final", "session_id": SID, "history_id": 7,
               "final": {"text": "Push the fix. ", "raw_text": "Push the fix.", "language": "en",
                         "edits": [{"stage": "rules", "rule": "trailing_space", "raw_span": {"start": 13, "end": 13},
                                    "before": "", "after": " "}]},
               "timings": {"audio_ms": 2100, "queue_ms": 1, "load_wait_ms": 0, "engine_ms": 180, "attempts": 1,
                           "pipeline_ms": 2, "stop_to_final_ms": 190},
               "trace": trace_json()}),
    );
}

#[test]
fn dictionary_edit_names_its_term_and_score() {
    assert_wire(
        &Edit {
            stage: Stage::Dictionary,
            rule: "fuzzy".into(),
            term: Some("hyprland".into()),
            raw_span: 0..10,
            before: "hyper land".into(),
            after: "Hyprland".into(),
            score: Some(0.25),
        },
        json!({"stage": "dictionary", "rule": "fuzzy", "term": "hyprland", "raw_span": {"start": 0, "end": 10},
               "before": "hyper land", "after": "Hyprland", "score": 0.25}),
    );
}

#[test]
fn error_and_cancelled() {
    let mut msg = ErrorMsg::new(Some(sid()), ErrorCode::ModelUnavailable, "model is downloading");
    msg.detail = Some(json!({"reason": "downloading", "bytes": 10, "total": 100}));
    assert_wire(
        &ServerMsg::Error(msg),
        json!({"type": "error", "session_id": SID, "code": "model_unavailable", "message": "model is downloading",
               "detail": {"reason": "downloading", "bytes": 10, "total": 100}, "retryable": false}),
    );
    assert_wire(
        &ServerMsg::Error(ErrorMsg::new(None, ErrorCode::Protocol, "unsupported protocol 2")),
        json!({"type": "error", "code": "protocol", "message": "unsupported protocol 2", "retryable": false}),
    );
    assert_wire(
        &ServerMsg::Cancelled(CancelledMsg { session_id: sid() }),
        json!({"type": "cancelled", "session_id": SID}),
    );
}

#[test]
fn error_codes_are_retryable_exactly_as_specified() {
    let all = [
        ("protocol", false),
        ("bad_request", false),
        ("unauthorized", false),
        ("busy", true),
        ("too_long", false),
        ("bad_audio", true),
        ("model_unloaded", false),
        ("model_unavailable", false),
        ("timeout", true),
        ("engine_failed", true),
        ("oom", true),
        ("internal", true),
    ];
    for (name, retryable) in all {
        let code: ErrorCode = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(code.retryable(), retryable, "{name}");
        assert_eq!(ErrorMsg::new(None, code, "x").retryable, retryable, "{name}");
    }
}

#[test]
fn receivers_ignore_unknown_fields() {
    let msg: ServerMsg = serde_json::from_value(json!({
        "type": "cancelled", "session_id": SID, "added_in_a_later_version": true
    }))
    .unwrap();
    assert_eq!(msg, ServerMsg::Cancelled(CancelledMsg { session_id: sid() }));
}

#[test]
fn unknown_message_type_is_rejected() {
    assert!(serde_json::from_value::<ClientMsg>(json!({"type": "ping"})).is_err());
}

#[test]
fn api_bodies() {
    assert_wire(
        &ModelStatus {
            name: "large-v3-turbo".into(),
            state: ModelState::Downloading,
            revision: None,
            progress: Some(DownloadProgress { bytes: 1024, total: Some(4096) }),
            error: None,
        },
        json!({"name": "large-v3-turbo", "state": "downloading", "progress": {"bytes": 1024, "total": 4096}}),
    );
    assert_wire(
        &Device {
            id: "local:alice".into(),
            name: "alice".into(),
            kind: DeviceKind::Local,
            created_at: "2026-11-06T10:00:00Z".parse().unwrap(),
            last_seen: None,
        },
        json!({"id": "local:alice", "name": "alice", "kind": "local", "created_at": "2026-11-06T10:00:00Z"}),
    );
    assert_wire(
        &PairRedeemed { device_id: "d1".into(), token: Token::new("s3cret") },
        json!({"device_id": "d1", "token": "s3cret"}),
    );
    assert_wire(
        &Pairing { code: "ABCD2345".into(), expires_at: "2026-11-06T10:05:00Z".parse().unwrap() },
        json!({"code": "ABCD2345", "expires_at": "2026-11-06T10:05:00Z"}),
    );
    assert_wire(
        &Stats {
            sessions: 3,
            timings: [("server.engine_ms".to_owned(), Percentiles { count: 3, p50: 180, p95: 240 })].into(),
        },
        json!({"sessions": 3, "timings": {"server.engine_ms": {"count": 3, "p50": 180, "p95": 240}}}),
    );
    assert_wire(
        &ApiError { code: ApiErrorCode::RateLimited, message: "slow down".into() },
        json!({"code": "rate_limited", "message": "slow down"}),
    );
}

#[test]
fn history_row_of_an_unfinished_session_has_no_final() {
    assert_wire(
        &HistoryRow {
            id: 1,
            session_id: sid(),
            device: "local:alice".into(),
            profile: "default".into(),
            created_at: "2026-11-06T10:00:00Z".parse().unwrap(),
            model: None,
            revision: None,
            r#final: None,
            server_timings: None,
            client_timings: None,
        },
        json!({"id": 1, "session_id": SID, "device": "local:alice", "profile": "default",
               "created_at": "2026-11-06T10:00:00Z"}),
    );
}

#[test]
fn token_debug_output_is_redacted() {
    let token = Token::new("s3cret");
    assert_eq!(token.as_str(), "s3cret");
    assert!(!format!("{token:?}").contains("s3cret"));
}

#[test]
fn a_term_leaves_out_keys_at_their_default() {
    assert_wire(&Term::new("Hyprland"), json!({"text": "Hyprland"}));
    let term =
        Term {
            languages: "cs".into(), fuzzy: false, pin: true, notes: Some("composer".into()), ..Term::new("Dvořák")
        };
    assert_wire(&term, json!({"text": "Dvořák", "languages": "cs", "fuzzy": false, "pin": true, "notes": "composer"}));
}

#[test]
fn term_ids_go_into_routes_as_one_percent_encoded_segment() {
    assert_eq!(routes::with_id(routes::DICTIONARY_TERM, "c++"), "/api/dictionary/terms/c%2B%2B");
    assert_eq!(
        routes::with_id(routes::DICTIONARY_TERM_RESET, "c/c# ř"),
        "/api/dictionary/terms/c%2Fc%23%20%C5%99/reset"
    );
    assert_eq!(routes::with_id(routes::DICTIONARY_NEVER_RULE, "7"), "/api/dictionary/never/7");
}
