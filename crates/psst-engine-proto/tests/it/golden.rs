//! `golden/from-engine/` is written by the Python engine's pytest and decoded here;
//! `golden/to-engine/` is written here and decoded by pytest. CI fails if regenerating them
//! changes the committed files.

use std::{fs, io::Cursor, path::PathBuf};

use psst_engine_proto::{
    Bias, EngineErrorCode, FrameLimits, FromEngine, Load, LoadPhase, StreamRef, ToEngine, Transcribe, decode, encode,
    read_frame, samples_to_payload, write_frame,
};
use serde_json::json;

const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

fn golden_dir(direction: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engines/faster-whisper/tests/golden").join(direction)
}

fn read_golden(name: &str) -> FromEngine {
    let path = golden_dir("from-engine").join(name);
    let bytes = fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut reader = Cursor::new(&bytes[..]);
    let frame = read_frame(&mut reader, &FrameLimits::for_max_audio_s(900)).unwrap().unwrap();
    assert_eq!(reader.position() as usize, bytes.len(), "{name} holds more than one frame");
    assert!(frame.payload.is_empty(), "{name}: engine frames carry no payload");
    decode(&frame.header).unwrap_or_else(|err| panic!("{name}: {err}"))
}

#[test]
fn decodes_engine_frames() {
    let FromEngine::Hello(hello) = read_golden("hello.bin") else { panic!("hello.bin") };
    assert_eq!((hello.protocol, hello.engine.as_str()), (1, "faster-whisper"));
    assert!(hello.capabilities.bias);
    assert!(hello.capabilities.stream);
    assert_eq!(hello.capabilities.max_audio_s, 900);

    let FromEngine::Progress(download) = read_golden("progress-download.bin") else { panic!("progress-download.bin") };
    assert_eq!((download.phase, download.bytes, download.total), (LoadPhase::Download, Some(524_288), Some(1_048_576)));
    let FromEngine::Progress(load) = read_golden("progress-load.bin") else { panic!("progress-load.bin") };
    assert_eq!((load.phase, load.bytes, load.total), (LoadPhase::Load, None, None));

    let FromEngine::Loaded(loaded) = read_golden("loaded.bin") else { panic!("loaded.bin") };
    assert_eq!((loaded.model.as_str(), loaded.revision.as_str(), loaded.load_ms), ("large-v3-turbo", REVISION, 2400));

    let FromEngine::Result(result) = read_golden("result.bin") else { panic!("result.bin") };
    assert_eq!(result.id, 42);
    assert_eq!(result.timings.decode_ms, 180);
    assert_eq!(result.timings.bias_terms_used, 3);
    assert!(!result.timings.prompt_truncated);
    let trace = &result.trace;
    assert_eq!(
        (trace.engine.as_str(), trace.revision.as_str(), trace.path.as_str()),
        ("faster-whisper", REVISION, "batched")
    );
    assert_eq!(trace.prompt, "I'm updating the NixOS config. Hyprland, psst, t3code.");
    assert_eq!((&trace.options["beam_size"], &trace.options["batch_size"]), (&json!(5), &json!(8)));
    assert_eq!(trace.result.speech_ms, Some(5200));
    let segment = &trace.result.segments[0];
    assert_eq!(segment.text, " Push the fix to psst.");
    assert_eq!(segment.no_speech_prob, Some(0.01));
    let words = segment.words.as_ref().unwrap();
    assert_eq!(words.len(), 5);
    assert_eq!((words[4].text.as_str(), words[4].prob), (" psst.", 0.61));

    assert_eq!((result.timings.chunks, result.timings.chunks_reused), (0, 0));
    let FromEngine::Result(stream) = read_golden("stream-result.bin") else { panic!("stream-result.bin") };
    assert_eq!(stream.id, 44);
    assert_eq!((stream.timings.chunks, stream.timings.chunks_reused), (0, 0));

    let FromEngine::Error(unsupported) = read_golden("error-unsupported.bin") else { panic!("error-unsupported.bin") };
    assert_eq!((unsupported.id, unsupported.code, unsupported.retryable), (None, EngineErrorCode::Unsupported, false));

    let FromEngine::Error(oom) = read_golden("error-oom.bin") else { panic!("error-oom.bin") };
    assert_eq!((oom.id, oom.code, oom.retryable), (Some(43), EngineErrorCode::Oom, true));
}

#[test]
fn every_engine_frame_decodes() {
    let mut names: Vec<_> = fs::read_dir(golden_dir("from-engine"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "error-oom.bin",
            "error-unsupported.bin",
            "hello.bin",
            "loaded.bin",
            "progress-download.bin",
            "progress-load.bin",
            "result.bin",
            "stream-result.bin"
        ]
    );
    for name in names {
        read_golden(&name);
    }
}

#[test]
fn writes_server_frames() {
    let dir = golden_dir("to-engine");
    fs::create_dir_all(&dir).unwrap();
    let pattern = [0.0, 0.25, -0.25, 0.5, -0.5, 1.0, -1.0, 0.125];
    let samples: Vec<f32> = pattern.iter().copied().cycle().take(1600).collect();
    let frames = [
        (
            "load.bin",
            ToEngine::Load(Load {
                model: "large-v3-turbo".into(),
                revision: Some(REVISION.into()),
                device: "cuda".into(),
                compute_type: "int8_float16".into(),
            }),
            Vec::new(),
        ),
        (
            "transcribe.bin",
            ToEngine::Transcribe(Transcribe {
                id: 42,
                language: "en".into(),
                bias: Bias {
                    style: "I'm updating the NixOS config.".into(),
                    terms: vec!["Hyprland".into(), "psst".into(), "t3code".into(), "Dvořák".into()],
                },
                options: json!({
                    "path": "batched", "beam_size": 5, "word_timestamps": true, "vad_filter": true,
                    "terms_as": "prompt", "batch_size": 8
                }),
            }),
            samples_to_payload(&samples),
        ),
        (
            "open.bin",
            ToEngine::Open(Transcribe {
                id: 43,
                language: "cs".into(),
                bias: Bias { style: "Upravuji konfiguraci NixOS.".into(), terms: vec!["Hyprland".into()] },
                options: json!({
                    "path": "batched", "beam_size": 5, "word_timestamps": true, "vad_filter": true,
                    "terms_as": "prompt", "batch_size": 8
                }),
            }),
            Vec::new(),
        ),
        ("append.bin", ToEngine::Append(StreamRef { id: 43 }), samples_to_payload(&samples[..320])),
        ("close.bin", ToEngine::Close(StreamRef { id: 43 }), Vec::new()),
        ("discard.bin", ToEngine::Discard(StreamRef { id: 43 }), Vec::new()),
        ("shutdown.bin", ToEngine::Shutdown, Vec::new()),
    ];
    for (name, msg, payload) in frames {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &encode(&msg), &payload).unwrap();
        fs::write(dir.join(name), bytes).unwrap();
    }
}
