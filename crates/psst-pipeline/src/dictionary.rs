use std::ops::Range;

use psst_dict::{Dictionary, FuzzyToken, Outcome, Term};
use psst_proto::{Edit, Stage};

use crate::draft::Draft;

pub(crate) fn apply(draft: &mut Draft, dict: &Dictionary, language: &str) {
    exact(draft, dict, language);
    fuzzy(draft, dict, language);
}

fn exact(draft: &mut Draft, dict: &Dictionary, language: &str) {
    let matches = dict.exact_matches(&draft.raw, language, |range| {
        draft.tokens[draft.overlapping(&range)].iter().all(|t| !t.removed)
    });
    let mut edits = Vec::new();
    // Back to front, so the raw offsets of earlier matches still index their tokens' text.
    for m in matches.iter().rev() {
        let tokens = draft.overlapping(&m.range);
        match m.outcome {
            Outcome::Ignore => {}
            Outcome::Keep => draft.tokens[tokens].iter_mut().for_each(|t| t.protected = true),
            Outcome::Replace => edits.push(replace(draft, tokens, m.range.clone(), m.term, "exact", None)),
        }
    }
    edits.reverse();
    draft.edits.extend(edits);
}

fn fuzzy(draft: &mut Draft, dict: &Dictionary, language: &str) {
    let tokens: Vec<FuzzyToken> = draft
        .tokens
        .iter()
        .map(|t| FuzzyToken {
            range: t.raw.clone(),
            prob: t.prob,
            // Matches index the raw text, so a token must still read as it did there.
            available: !t.removed && !t.protected && draft.raw[t.raw.clone()] == t.text,
        })
        .collect();
    let matches = dict.fuzzy_matches(&draft.raw, &tokens, language);
    let mut edits = Vec::new();
    for m in matches.iter().rev() {
        if draft.raw[m.range.clone()] == m.term.text {
            draft.tokens[m.tokens.clone()].iter_mut().for_each(|t| t.protected = true);
            continue;
        }
        edits.push(replace(draft, m.tokens.clone(), m.range.clone(), m.term, "fuzzy", Some(m.score)));
    }
    edits.reverse();
    draft.edits.extend(edits);
}

fn replace(
    draft: &mut Draft,
    tokens: Range<usize>,
    range: Range<usize>,
    term: &Term,
    rule: &str,
    score: Option<f32>,
) -> Edit {
    let (first, last) = (tokens.start, tokens.end - 1);
    let prefix = &draft.tokens[first].text[..range.start - draft.tokens[first].raw.start];
    let suffix = &draft.tokens[last].text[range.end - draft.tokens[last].raw.start..];
    let text = format!("{prefix}{}{suffix}", term.text);
    let merged = &draft.tokens[tokens.clone()];
    let raw_end = merged[merged.len() - 1].raw.end;
    let prob = merged.iter().map(|t| t.prob).reduce(|a, b| Some(a?.min(b?))).flatten();
    let start_ms = merged.iter().map(|t| t.start_ms).reduce(|a, b| Some(a?.min(b?))).flatten();
    let end_ms = merged.iter().map(|t| t.end_ms).reduce(|a, b| Some(a?.max(b?))).flatten();
    for token in &mut draft.tokens[first + 1..=last] {
        token.text.clear();
        token.space.clear();
        token.removed = true;
    }
    let token = &mut draft.tokens[first];
    token.text = text;
    token.raw.end = raw_end;
    (token.prob, token.start_ms, token.end_ms) = (prob, start_ms, end_ms);
    token.protected = true;
    Edit {
        stage: Stage::Dictionary,
        rule: rule.into(),
        term: Some(term.id.clone()),
        before: draft.raw[range.clone()].to_owned(),
        raw_span: range,
        after: term.text.clone(),
        score,
    }
}
