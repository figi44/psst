//! The built-in post-processing rules (plan 8), applied in the profile's order. Rules leave
//! protected tokens alone.

use psst_config::Rule;
use psst_proto::{Edit, Stage};

use crate::draft::Draft;

pub(crate) fn apply(draft: &mut Draft, rules: &[Rule], fillers: &[String]) {
    for rule in rules {
        match rule {
            Rule::Fillers => remove_fillers(draft, fillers),
            Rule::Whitespace => collapse_spaces(draft),
            Rule::Capitalize => capitalize(draft),
            Rule::TrailingSpace => trailing_space(draft),
        }
    }
}

fn edit(rule: &str, raw_span: std::ops::Range<usize>, before: &str, after: &str) -> Edit {
    Edit {
        stage: Stage::Rules,
        rule: rule.into(),
        term: None,
        raw_span,
        before: before.into(),
        after: after.into(),
        score: None,
    }
}

/// `fillers`: removes a token that is a filler word, ignoring case, together with the one comma
/// attached to it ("I, um, think" → "I,  think"). The spacing stays for `whitespace` to collapse.
fn remove_fillers(draft: &mut Draft, fillers: &[String]) {
    let fillers: Vec<String> = fillers.iter().map(|f| f.trim().to_lowercase()).filter(|f| !f.is_empty()).collect();
    for token in draft.tokens.iter_mut().filter(|t| !t.removed && !t.protected) {
        let lower = token.text.to_lowercase();
        if fillers.iter().any(|f| *f == lower.strip_suffix(',').unwrap_or(&lower)) {
            // One edit for a token an earlier rule changed (`capitalize` before `fillers`).
            let before = match draft.edits.iter().position(|e| e.stage == Stage::Rules && e.raw_span == token.raw) {
                Some(earlier) => draft.edits.remove(earlier).before,
                None => token.text.clone(),
            };
            draft.edits.push(edit("fillers", token.raw.clone(), &before, ""));
            token.text.clear();
            token.removed = true;
        }
    }
}

/// `whitespace`: collapses each run of spaces between two tokens to one, and removes spaces before
/// the first token and after the last, including one an earlier rule appended. Spacing with any
/// other whitespace in it, and punctuation, are left alone.
fn collapse_spaces(draft: &mut Draft) {
    let mut edits = Vec::new();
    let mut gap: Vec<usize> = Vec::new();
    let mut seen_text = false;
    for i in 0..draft.tokens.len() {
        gap.push(i);
        if !draft.tokens[i].removed {
            collapse_gap(draft, &gap, usize::from(seen_text), false, &mut edits);
            seen_text = true;
            gap.clear();
        }
    }
    collapse_gap(draft, &gap, 0, true, &mut edits);
    edits.sort_by_key(|e: &Edit| e.raw_span.start);
    draft.edits.extend(edits);
}

/// Shortens the spacing of the tokens in `gap`, and at the end the appended tail, to `keep` spaces,
/// kept at the start of the last token's spacing.
fn collapse_gap(draft: &mut Draft, gap: &[usize], keep: usize, at_end: bool, edits: &mut Vec<Edit>) {
    let only_spaces = |s: &str| s.bytes().all(|b| b == b' ');
    let spaced: Vec<usize> = gap.iter().copied().filter(|&i| !draft.tokens[i].space.is_empty()).collect();
    if !spaced.iter().all(|&i| only_spaces(&draft.tokens[i].space)) || (at_end && !only_spaces(&draft.tail)) {
        return;
    }
    if at_end && !draft.tail.is_empty() {
        let end = draft.raw.len();
        edits.push(edit("whitespace", end..end, &draft.tail, ""));
        draft.tail.clear();
    }
    let mut keep = keep;
    for &i in spaced.iter().rev() {
        let token = &mut draft.tokens[i];
        let kept = keep.min(token.space.len());
        keep -= kept;
        if kept == token.space.len() {
            continue;
        }
        let raw_span = match draft.raw[token.raw_space.clone()] == token.space {
            true => token.raw_space.start + kept..token.raw_space.end,
            false => token.raw_space.clone(),
        };
        edits.push(edit("whitespace", raw_span, &token.space[kept..], ""));
        token.space.truncate(kept);
    }
}

/// `capitalize`: uppercases the first letter of the text, unless the first token with a letter or
/// digit in it is protected or starts with a digit.
fn capitalize(draft: &mut Draft) {
    let Some(token) = draft.tokens.iter_mut().find(|t| !t.removed && t.text.chars().any(char::is_alphanumeric)) else {
        return;
    };
    if token.protected {
        return;
    }
    let Some((at, c)) = token.text.char_indices().find(|(_, c)| c.is_alphanumeric()) else { return };
    if !c.is_lowercase() {
        return;
    }
    let after = format!("{}{}{}", &token.text[..at], c.to_uppercase(), &token.text[at + c.len_utf8()..]);
    draft.edits.push(edit("capitalize", token.raw.clone(), &token.text, &after));
    token.text = after;
}

/// `trailing_space`: appends a space to non-empty text.
fn trailing_space(draft: &mut Draft) {
    if draft.has_text() {
        let end = draft.raw.len();
        draft.edits.push(edit("trailing_space", end..end, "", " "));
        draft.tail.push(' ');
    }
}
