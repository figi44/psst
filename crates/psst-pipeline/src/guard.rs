use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use psst_proto::{Edit, EngineResult, Stage};

use crate::draft::Draft;

const NO_SPEECH_PROB: f32 = 0.6;
const WEAK_SPEECH_MS: u32 = 500;

static PHRASES: LazyLock<HashMap<&str, HashSet<String>>> = LazyLock::new(|| {
    let parse =
        |file: &str| file.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).map(normalise).collect();
    HashMap::from([
        ("en", parse(include_str!("../data/hallucinations/en.txt"))),
        ("cs", parse(include_str!("../data/hallucinations/cs.txt"))),
    ])
});

fn normalise(text: &str) -> String {
    let text = text.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'')).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

fn is_known_phrase(language: &str, text: &str) -> bool {
    PHRASES.get(language).is_some_and(|phrases| phrases.contains(&normalise(text)))
}

pub(crate) fn apply(draft: &mut Draft, result: &EngineResult, language: &str) {
    let rules: Vec<Option<&str>> = draft
        .segments
        .iter()
        .map(|span| {
            if result.speech_ms == Some(0) {
                return Some("no_speech");
            }
            let segment = &result.segments[span.index];
            let weak = match segment.no_speech_prob {
                Some(prob) => prob > NO_SPEECH_PROB,
                None => result.speech_ms.is_some_and(|ms| ms < WEAK_SPEECH_MS),
            };
            (weak && is_known_phrase(language, &segment.text)).then_some("hallucination")
        })
        .collect();

    for (s, rule) in rules.iter().enumerate() {
        let Some(rule) = rule else { continue };
        let tokens = draft.segments[s].tokens.clone();
        let kept_before = rules[..s].iter().any(Option::is_none);
        let kept_after = rules[s + 1..].iter().any(Option::is_none);
        // At what would become the start of the text, take the space after instead of before.
        let takes_space_after = !kept_before && kept_after;
        let first = &draft.tokens[tokens.start];
        let last = &draft.tokens[tokens.end - 1];
        let span = match takes_space_after {
            true => first.raw.start..draft.tokens[tokens.end].raw_space.end,
            false => first.raw_space.start..last.raw.end,
        };
        draft.edits.push(Edit {
            stage: Stage::Guard,
            rule: rule.to_string(),
            term: None,
            before: draft.raw[span.clone()].to_owned(),
            raw_span: span,
            after: String::new(),
            score: None,
        });
        for token in &mut draft.tokens[tokens.clone()] {
            token.text.clear();
            token.space.clear();
            token.removed = true;
        }
        if takes_space_after {
            draft.tokens[tokens.end].space.clear();
        }
    }
}
