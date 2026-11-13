//! Plan 7.1's performance target: a 200-word dictation against 500 terms in under 5 ms.

use std::time::{Duration, Instant};

use psst_config::{ConfigFile, DictionaryFile, ProfilesFile};
use psst_dict::{Dictionary, common_words};
use psst_pipeline::{PipelineOptions, run};
use psst_proto::{EngineResult, EngineTrace, Segment, Word};

/// 500 made-up terms, each with two `sounds_like` spellings, a quarter of them pinned.
fn dictionary() -> Dictionary {
    let toml: String = (0..500)
        .map(|i| {
            format!(
                "[[terms]]\ntext = \"Zork{i}ix\"\nsounds_like = [\"zork {i} icks\", \"sork {i}\"]\npin = {}\n",
                i % 4 == 0
            )
        })
        .collect();
    Dictionary::from_file(&DictionaryFile::from_toml_str(&toml).unwrap()).unwrap()
}

/// Ten segments of 20 items with word timestamps: common English words, and a term spelling of
/// two or three words as every fifth item. 260 words in all.
fn dictation() -> EngineTrace {
    let common = common_words("en");
    let words: Vec<String> = (0..200)
        .map(|i| match i % 10 {
            0 => format!("zork {} icks,", i * 7 % 500),
            5 => format!("SORK {}.", i * 13 % 500),
            _ => common[i * 37 % 1000].to_owned(),
        })
        .collect();
    let segments = words
        .chunks(20)
        .enumerate()
        .map(|(s, chunk)| {
            let start = s as u32 * 6000;
            let words: Vec<Word> = chunk
                .iter()
                .enumerate()
                .map(|(w, text)| Word {
                    text: format!(" {text}"),
                    start_ms: start + w as u32 * 300,
                    end_ms: start + w as u32 * 300 + 250,
                    prob: 0.9,
                })
                .collect();
            Segment {
                text: words.iter().map(|w| w.text.as_str()).collect(),
                start_ms: start,
                end_ms: start + 6000,
                no_speech_prob: Some(0.01),
                words: Some(words),
            }
        })
        .collect();
    EngineTrace {
        engine: "fake".into(),
        model: "m".into(),
        revision: "r".into(),
        path: "sequential".into(),
        options: serde_json::json!({}),
        prompt: String::new(),
        result: EngineResult { language: "en".into(), speech_ms: Some(60_000), segments },
    }
}

#[test]
fn a_200_word_dictation_against_500_terms_takes_under_5_ms() {
    let dict = dictionary();
    let trace = dictation();
    let profile = ProfilesFile::default().resolve("default").unwrap();
    let opts = PipelineOptions::default();

    let out = run(&trace, &profile, &dict, &opts);
    assert_eq!(out.raw_text.split_whitespace().count(), 260);
    assert_eq!(out.edits.iter().filter(|e| e.term.is_some()).count(), 40);
    assert!(out.text.contains("Zork70ix,") && out.text.contains("Zork195ix."), "{}", out.text);

    // The best of several runs, so a busy machine doesn't fail the test.
    let best = (0..20)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(run(&trace, &profile, &dict, &opts));
            start.elapsed()
        })
        .min()
        .unwrap();
    assert!(best < Duration::from_millis(5), "took {best:?}");
}
