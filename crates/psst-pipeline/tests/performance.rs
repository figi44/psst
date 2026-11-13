//! Plan 7.1's performance target: a 200-word dictation against 500 terms in under 5 ms, with exact
//! spellings, and with fuzzy matching at its most expensive.

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

    let best = best_time(|| run(&trace, &profile, &dict, &opts));
    assert!(best < Duration::from_millis(5), "took {best:?}");
}

/// The best of several runs, so a busy machine doesn't fail the test.
fn best_time<T>(mut f: impl FnMut() -> T) -> Duration {
    (0..20)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(f());
            start.elapsed()
        })
        .min()
        .unwrap()
}

/// 500 made-up pronounceable terms of letters only, so they all have Double Metaphone codes: two
/// to four syllables, every seventh one two words.
fn fuzzy_terms() -> Vec<String> {
    const SYLLABLES: [&str; 16] =
        ["ka", "lo", "mi", "tre", "zu", "pen", "dor", "vax", "shi", "qua", "bel", "nor", "fy", "gri", "sto", "lux"];
    (0..500)
        .map(|i: usize| {
            let parts = [i % 16, i / 16 % 16, (i * 7 + 3) % 16, (i * 11 + 5) % 16];
            let word: String = parts[..2 + i % 3].iter().map(|&p| SYLLABLES[p]).collect();
            let word = format!("{}{}", word[..1].to_uppercase(), &word[1..]);
            match i % 7 {
                0 => format!("{word} {}", SYLLABLES[i % 13]),
                _ => word,
            }
        })
        .collect()
}

/// 200 words: ordinary English, and every tenth word a term misheard, split in two or with one
/// letter changed.
fn fuzzy_dictation(terms: &[String]) -> String {
    let common = common_words("en");
    let words: Vec<String> = (0..200)
        .map(|i| match i % 10 {
            0 => {
                let term = terms[i * 37 % terms.len()].to_lowercase();
                let at = term.len() / 2;
                match i % 20 {
                    0 => format!("{} {}", &term[..at], &term[at..]),
                    _ => term.replacen(['a', 'o', 'i'], "e", 1),
                }
            }
            _ => common[i * 53 % 3000].to_owned(),
        })
        .collect();
    words.join(" ")
}

#[test]
fn fuzzy_matching_a_200_word_dictation_against_500_terms_takes_under_5_ms() {
    let terms = fuzzy_terms();
    let toml: String = terms.iter().map(|t| format!("[[terms]]\ntext = \"{t}\"\n")).collect();
    let dict = Dictionary::from_file(&DictionaryFile::from_toml_str(&toml).unwrap()).unwrap();
    let spoken = fuzzy_dictation(&terms);
    let profile = ProfilesFile::default().resolve("default").unwrap();
    let opts = PipelineOptions::default();
    // Without probabilities (the medium band), and unsure of every word (the low band), which
    // allows the most edits and so scans the most terms.
    for trace in [said(&spoken, None), said(&spoken, Some(0.3))] {
        let out = run(&trace, &profile, &dict, &opts);
        assert!(out.raw_text.split_whitespace().count() >= 200);
        let fuzzy = out.edits.iter().filter(|e| e.rule == "fuzzy").count();
        assert!(fuzzy >= 10, "{fuzzy} fuzzy edits: {}", out.text);
        let best = best_time(|| run(&trace, &profile, &dict, &opts));
        assert!(best < Duration::from_millis(5), "took {best:?}");
    }
}

/// One segment of English, its words with probability `prob` if any.
fn said(text: &str, prob: Option<f32>) -> EngineTrace {
    let words = prob.map(|prob| {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word { text: format!(" {w}"), start_ms: i as u32 * 300, end_ms: i as u32 * 300 + 250, prob })
            .collect()
    });
    let segment = Segment { text: format!(" {text}"), start_ms: 0, end_ms: 60_000, no_speech_prob: Some(0.01), words };
    EngineTrace {
        engine: "fake".into(),
        model: "m".into(),
        revision: "r".into(),
        path: "sequential".into(),
        options: serde_json::json!({}),
        prompt: String::new(),
        result: EngineResult { language: "en".into(), speech_ms: Some(60_000), segments: vec![segment] },
    }
}
