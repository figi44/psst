use psst_config::ProfilesFile;
use psst_dict::Dictionary;
use psst_pipeline::{PipelineOptions, run};
use psst_proto::{EngineResult, EngineTrace, Segment};

fn trace(language: &str, texts: &[&str]) -> EngineTrace {
    EngineTrace {
        engine: "fake".into(),
        model: "m".into(),
        revision: "r".into(),
        path: "sequential".into(),
        options: serde_json::json!({}),
        prompt: String::new(),
        result: EngineResult {
            language: language.into(),
            speech_ms: Some(1000),
            segments: texts
                .iter()
                .map(|text| Segment { text: (*text).into(), start_ms: 0, end_ms: 0, no_speech_prob: None, words: None })
                .collect(),
        },
    }
}

#[test]
fn raw_text_is_the_trimmed_segments_joined_by_single_spaces() {
    let profile = ProfilesFile::default().resolve("default").unwrap();
    let out = run(
        &trace("cs", &[" Push the fix.", "  ", " Then  deploy. "]),
        &profile,
        &Dictionary::default(),
        &PipelineOptions::default(),
    );
    assert_eq!(out.raw_text, "Push the fix. Then  deploy.");
    assert_eq!(out.language, "cs");
}

#[test]
fn the_first_stub_returns_raw_text_unchanged() {
    let profile = ProfilesFile::default().resolve("default").unwrap();
    let out = run(&trace("en", &[" um hello"]), &profile, &Dictionary::default(), &PipelineOptions { dictionary: false });
    assert_eq!(out.text, out.raw_text);
    assert!(out.edits.is_empty());
}

#[test]
fn no_segments_give_empty_text() {
    let profile = ProfilesFile::default().resolve("default").unwrap();
    let out = run(&trace("en", &[]), &profile, &Dictionary::default(), &PipelineOptions::default());
    assert_eq!(out.text, "");
}
