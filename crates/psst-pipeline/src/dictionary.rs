//! The dictionary stage (plan 6.4 step 4, 7.2): exact spellings now, fuzzy matching in Phase 2.

use psst_dict::{Dictionary, Outcome};
use psst_proto::{Edit, Stage};

use crate::draft::Draft;

pub(crate) fn apply(draft: &mut Draft, dict: &Dictionary, language: &str) {
    exact(draft, dict, language);
    // Phase 2 adds fuzzy matching (7.2 steps 3–7) here: English sessions, `fuzzy` terms, over spans
    // of tokens that are neither removed nor protected, using the tokens' `prob` for the
    // confidence gate. Every accepted match protects its tokens, like an exact one.
}

/// Step 2: replaces only the matched characters, so "hyper land." becomes "Hyprland.", and
/// protects every token a match accepts. A match spanning several tokens merges them into one.
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
            Outcome::Replace => {
                let (first, last) = (tokens.start, tokens.end - 1);
                let prefix = &draft.tokens[first].text[..m.range.start - draft.tokens[first].raw.start];
                let suffix = &draft.tokens[last].text[m.range.end - draft.tokens[last].raw.start..];
                let text = format!("{prefix}{}{suffix}", m.term.text);
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
                edits.push(Edit {
                    stage: Stage::Dictionary,
                    rule: "exact".into(),
                    term: Some(m.term.id.clone()),
                    raw_span: m.range.clone(),
                    before: draft.raw[m.range.clone()].to_owned(),
                    after: m.term.text.clone(),
                    score: None,
                });
            }
        }
    }
    edits.reverse();
    draft.edits.extend(edits);
}
