//! The scripted fake engine, driven in-process through its stdin/stdout streams.

use std::{io::Cursor, path::Path};

use psst_engine_proto::{
    Bias, EngineErrorCode, FrameLimits, FromEngine, Load, LoadPhase, Progress, ToEngine, Transcribe, decode, encode,
    fake::{self, Exit, LoadOutcome, LoadScript, Script, TranscribeStep},
    read_frame, samples_to_payload, write_frame,
};
use serde_json::json;

fn load() -> ToEngine {
    ToEngine::Load(Load {
        model: "large-v3-turbo".into(),
        revision: None,
        device: "cpu".into(),
        compute_type: "int8".into(),
    })
}

fn transcribe(id: u64, word_timestamps: bool) -> ToEngine {
    ToEngine::Transcribe(Transcribe {
        id,
        language: "en".into(),
        bias: Bias { style: "I'm updating the NixOS config.".into(), terms: vec!["Hyprland".into(), "psst".into()] },
        options: json!({"path": "sequential", "word_timestamps": word_timestamps}),
    })
}

/// Two seconds of audio.
fn audio() -> Vec<u8> {
    samples_to_payload(&[0.0; 32_000])
}

fn input(requests: &[(ToEngine, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (request, payload) in requests {
        write_frame(&mut out, &encode(request), payload).unwrap();
    }
    out
}

fn run(script: &Script, log: Option<&Path>, input: Vec<u8>) -> (Exit, Vec<FromEngine>) {
    let mut output = Vec::new();
    let exit = fake::run(script, log, &mut Cursor::new(input), &mut output).unwrap();
    let mut reader = Cursor::new(output);
    let mut replies = Vec::new();
    while let Some(frame) = read_frame(&mut reader, &FrameLimits::default()).unwrap() {
        replies.push(decode(&frame.header).unwrap());
    }
    (exit, replies)
}

#[test]
fn announces_itself_loads_transcribes_and_shuts_down() {
    let script = Script {
        load: LoadScript {
            progress: vec![
                Progress { phase: LoadPhase::Download, bytes: Some(1), total: Some(2) },
                Progress { phase: LoadPhase::Load, bytes: None, total: None },
            ],
            delay_ms: 5,
            outcome: LoadOutcome::Loaded { revision: Some("rev1".into()) },
        },
        transcribe: vec![TranscribeStep::Result {
            text: "Push the fix to psst.".into(),
            delay_ms: 0,
            no_speech_prob: None,
            speech_ms: None,
        }],
        ..Script::default()
    };
    let (exit, replies) = run(
        &script,
        None,
        input(&[(load(), vec![]), (transcribe(7, true), audio()), (ToEngine::Shutdown, vec![])]),
    );
    assert_eq!(exit, Exit::Shutdown);

    let [FromEngine::Hello(hello), FromEngine::Progress(download), FromEngine::Progress(_), FromEngine::Loaded(loaded), FromEngine::Result(result)] =
        &replies[..]
    else {
        panic!("unexpected replies: {replies:#?}");
    };
    assert_eq!((hello.protocol, hello.capabilities.max_audio_s), (1, 900));
    assert_eq!(download.phase, LoadPhase::Download);
    assert_eq!((loaded.revision.as_str(), loaded.load_ms), ("rev1", 5));

    assert_eq!(result.id, 7);
    let trace = &result.trace;
    assert_eq!(trace.revision, "rev1");
    assert_eq!(trace.prompt, "I'm updating the NixOS config. Hyprland, psst.");
    assert_eq!(trace.result.speech_ms, Some(2000));
    let [segment] = &trace.result.segments[..] else { panic!("{trace:#?}") };
    assert_eq!(segment.text, " Push the fix to psst.");
    assert_eq!(segment.end_ms, 2000);
    let words: Vec<_> = segment.words.as_ref().unwrap().iter().map(|w| w.text.as_str()).collect();
    assert_eq!(words, [" Push", " the", " fix", " to", " psst."]);
    assert_eq!(result.timings.bias_terms_used, 2);
}

#[test]
fn words_only_with_word_timestamps_and_empty_text_means_nothing_heard() {
    let script = Script {
        transcribe: vec![TranscribeStep::Result { text: "".into(), delay_ms: 0, no_speech_prob: None, speech_ms: None }],
        ..Script::default()
    };
    let (_, replies) = run(&script, None, input(&[(load(), vec![]), (transcribe(1, false), audio()), (transcribe(2, false), audio())]));
    let results: Vec<_> = replies
        .iter()
        .filter_map(|r| match r {
            FromEngine::Result(result) => Some(&result.trace.result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].segments.is_empty());
    assert_eq!(results[0].speech_ms, Some(0));
    // Steps ran out: the default answer, without words since word_timestamps is off.
    assert_eq!(results[1].segments[0].text, " fake transcript");
    assert!(results[1].segments[0].words.is_none());
}

#[test]
fn answers_unknown_types_malformed_requests_and_early_transcribes_with_errors() {
    let mut stream = input(&[(transcribe(3, false), audio())]);
    write_frame(&mut stream, br#"{"type":"frobnicate","id":4}"#, b"").unwrap();
    write_frame(&mut stream, br#"{"type":"load"}"#, b"").unwrap();
    let (exit, replies) = run(&Script::default(), None, stream);
    assert_eq!(exit, Exit::EndOfInput);

    let codes: Vec<_> = replies
        .iter()
        .skip(1)
        .map(|r| match r {
            FromEngine::Error(e) => (e.id, e.code),
            other => panic!("expected an error, got {other:?}"),
        })
        .collect();
    assert_eq!(
        codes,
        [(Some(3), EngineErrorCode::NotLoaded), (Some(4), EngineErrorCode::Unsupported), (None, EngineErrorCode::BadRequest)]
    );
}

#[test]
fn scripted_steps_carry_on_across_restarts_through_the_request_log() {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("script.json");
    let log = fake::request_log_path(&script_path);
    let script = Script {
        transcribe: vec![
            TranscribeStep::Exit { code: 9, delay_ms: 0 },
            TranscribeStep::Error {
                code: EngineErrorCode::Oom,
                message: "out of memory".into(),
                retryable: true,
                delay_ms: 0,
            },
        ],
        ..Script::default()
    };

    let (exit, _) = run(&script, Some(&log), input(&[(load(), vec![]), (transcribe(1, false), audio())]));
    assert_eq!(exit, Exit::Scripted(9), "first engine crashes on its first transcribe");

    let (_, replies) = run(&script, Some(&log), input(&[(load(), vec![]), (transcribe(2, false), audio())]));
    assert!(
        matches!(replies.last(), Some(FromEngine::Error(e)) if e.code == EngineErrorCode::Oom && e.id == Some(2)),
        "restarted engine continues with the second step: {replies:#?}"
    );

    let logged = fake::read_request_log(&script_path).unwrap();
    let summary: Vec<_> = logged.iter().map(|r| (r.header["type"].as_str().unwrap(), r.samples)).collect();
    assert_eq!(summary, [("load", 0), ("transcribe", 32_000), ("load", 0), ("transcribe", 32_000)]);
}

#[test]
fn a_scripted_load_error_leaves_the_engine_unloaded() {
    let script = Script {
        load: LoadScript {
            outcome: LoadOutcome::Error { code: EngineErrorCode::LoadFailed, message: "no such model".into() },
            ..LoadScript::default()
        },
        ..Script::default()
    };
    let (_, replies) = run(&script, None, input(&[(load(), vec![]), (transcribe(1, false), audio())]));
    let codes: Vec<_> = replies
        .iter()
        .filter_map(|r| match r {
            FromEngine::Error(e) => Some(e.code),
            _ => None,
        })
        .collect();
    assert_eq!(codes, [EngineErrorCode::LoadFailed, EngineErrorCode::NotLoaded]);
}

#[test]
fn scripts_are_json_with_defaults_for_everything() {
    let script: Script = serde_json::from_value(json!({
        "load": {"outcome": {"kind": "error", "code": "load_failed", "message": "gone"}},
        "transcribe": [{"kind": "result", "text": "hi"}, {"kind": "exit", "code": 1}, {"kind": "hang"}]
    }))
    .unwrap();
    assert_eq!(script.hello.max_audio_s, 900);
    assert_eq!(script.transcribe.len(), 3);
    assert!(serde_json::from_value::<Script>(json!({"typo": 1})).is_err());
}
