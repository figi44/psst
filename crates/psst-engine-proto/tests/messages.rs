use std::fmt::Debug;

use psst_engine_proto::{
    Bias, Capabilities, DecodeError, EngineError, EngineErrorCode, EngineTimings, FromEngine, Hello, Load, LoadPhase,
    Loaded, Message, Progress, ToEngine, Transcribe, decode, encode, payload_to_samples, samples_to_payload,
};
use serde_json::{Value, json};

/// `msg` encodes to exactly `wire` and decodes back, and its `type_name` is a known type.
#[track_caller]
fn assert_wire<M: Message + PartialEq + Debug>(msg: M, wire: Value) {
    let header = encode(&msg);
    assert_eq!(serde_json::from_slice::<Value>(&header).unwrap(), wire);
    assert_eq!(wire["type"], msg.type_name());
    assert!(M::TYPES.contains(&msg.type_name()));
    assert_eq!(decode::<M>(&header).unwrap(), msg);
}

#[test]
fn server_to_engine_messages() {
    assert_wire(
        ToEngine::Load(Load {
            model: "large-v3-turbo".into(),
            revision: None,
            device: "cuda".into(),
            compute_type: "int8_float16".into(),
        }),
        json!({"type": "load", "model": "large-v3-turbo", "device": "cuda", "compute_type": "int8_float16"}),
    );
    assert_wire(
        ToEngine::Transcribe(Transcribe {
            id: 42,
            language: "en".into(),
            bias: Bias { style: "Hi.".into(), terms: vec!["psst".into()] },
            options: json!({"path": "sequential"}),
        }),
        json!({"type": "transcribe", "id": 42, "language": "en", "bias": {"style": "Hi.", "terms": ["psst"]},
               "options": {"path": "sequential"}}),
    );
    assert_wire(ToEngine::Shutdown, json!({"type": "shutdown"}));
}

#[test]
fn engine_to_server_messages() {
    assert_wire(
        FromEngine::Hello(Hello {
            protocol: 1,
            engine: "faster-whisper".into(),
            version: "0.1.0".into(),
            capabilities: Capabilities { bias: true, max_audio_s: 900 },
        }),
        json!({"type": "hello", "protocol": 1, "engine": "faster-whisper", "version": "0.1.0",
               "capabilities": {"bias": true, "max_audio_s": 900}}),
    );
    assert_wire(
        FromEngine::Progress(Progress { phase: LoadPhase::Download, bytes: Some(10), total: Some(100) }),
        json!({"type": "progress", "phase": "download", "bytes": 10, "total": 100}),
    );
    assert_wire(
        FromEngine::Progress(Progress { phase: LoadPhase::Load, bytes: None, total: None }),
        json!({"type": "progress", "phase": "load"}),
    );
    assert_wire(
        FromEngine::Loaded(Loaded { model: "large-v3-turbo".into(), revision: "abc".into(), load_ms: 2400 }),
        json!({"type": "loaded", "model": "large-v3-turbo", "revision": "abc", "load_ms": 2400}),
    );
    assert_wire(
        FromEngine::Error(EngineError {
            id: Some(43),
            code: EngineErrorCode::Oom,
            message: "out of memory".into(),
            retryable: true,
        }),
        json!({"type": "error", "id": 43, "code": "oom", "message": "out of memory", "retryable": true}),
    );
}

#[test]
fn result_timings_shape() {
    let timings = EngineTimings { decode_ms: 180, prompt_tokens: 19, bias_terms_used: 3, prompt_truncated: false };
    assert_eq!(
        serde_json::to_value(timings).unwrap(),
        json!({"decode_ms": 180, "prompt_tokens": 19, "bias_terms_used": 3, "prompt_truncated": false})
    );
}

#[test]
fn an_unknown_type_is_reported_as_unsupported_not_malformed() {
    let err = decode::<ToEngine>(br#"{"type":"frobnicate","x":1}"#).unwrap_err();
    assert!(matches!(&err, DecodeError::UnknownType(t) if t == "frobnicate"), "{err:?}");

    // `hello` is a known type in the other direction only.
    let err = decode::<ToEngine>(br#"{"type":"hello"}"#).unwrap_err();
    assert!(matches!(&err, DecodeError::UnknownType(t) if t == "hello"), "{err:?}");
}

#[test]
fn known_types_with_bad_fields_and_non_json_are_malformed() {
    for header in [&br#"{"type":"load","model":"x"}"#[..], b"not json", br#"{"no":"type"}"#, br#"[1,2]"#, b"\xff"] {
        let err = decode::<ToEngine>(header).unwrap_err();
        assert!(matches!(err, DecodeError::Malformed(_)), "{:?}: {err:?}", String::from_utf8_lossy(header));
    }
}

#[test]
fn engine_errors_default_to_not_retryable() {
    let msg: FromEngine = decode(br#"{"type":"error","code":"unsupported","message":"no"}"#).unwrap();
    assert_eq!(
        msg,
        FromEngine::Error(EngineError {
            id: None,
            code: EngineErrorCode::Unsupported,
            message: "no".into(),
            retryable: false
        })
    );
}

#[test]
fn audio_payload_is_f32_little_endian() {
    let samples = [0.0, 0.5, -1.0, 1.0e-3];
    let payload = samples_to_payload(&samples);
    assert_eq!(payload.len(), 16);
    assert_eq!(&payload[4..8], &0.5f32.to_le_bytes());
    assert_eq!(payload_to_samples(&payload).unwrap(), samples);
    assert!(payload_to_samples(&payload[..15]).is_none());
}
