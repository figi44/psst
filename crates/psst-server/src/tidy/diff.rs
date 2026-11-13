use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    /// `same`, `fmt` (the same word, recased or repunctuated), `del` or `ins`.
    pub kind: &'static str,
    pub text: String,
}

/// Keeps the table small: a dictation is a few hundred words.
const MAX_CELLS: usize = 1_000_000;

/// The tidied text, with the transcript's dropped words put back in as `del`. Words that only
/// gained a capital or punctuation count as kept, so the diff shows the edits that matter.
pub fn words(before: &str, after: &str) -> Option<Vec<Segment>> {
    let old: Vec<&str> = before.split_whitespace().collect();
    let new = chunks(after.trim());
    let (n, m) = (old.len(), new.len());
    if (n + 1) * (m + 1) > MAX_CELLS {
        return None;
    }
    let old_keys: Vec<String> = old.iter().map(|w| key(w)).collect();
    let new_keys: Vec<String> = new.iter().map(|(w, _)| key(w)).collect();
    let same = |i: usize, j: usize| {
        if old_keys[i].is_empty() && new_keys[j].is_empty() { old[i] == new[j].0 } else { old_keys[i] == new_keys[j] }
    };
    let at = |i: usize, j: usize| i * (m + 1) + j;
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] =
                if same(i, j) { lcs[at(i + 1, j + 1)] + 1 } else { lcs[at(i + 1, j)].max(lcs[at(i, j + 1)]) };
        }
    }

    let mut out: Vec<Segment> = Vec::new();
    let mut push = |kind: &'static str, text: String| match out.last_mut() {
        Some(last) if last.kind == kind => last.text.push_str(&text),
        _ => out.push(Segment { kind, text }),
    };
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && same(i, j) {
            push(if old[i] == new[j].0 { "same" } else { "fmt" }, format!("{}{}", new[j].0, new[j].1));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
            push("del", format!("{} ", old[i]));
            i += 1;
        } else {
            push("ins", format!("{}{}", new[j].0, new[j].1));
            j += 1;
        }
    }
    Some(out)
}

fn key(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Each word with the whitespace after it, so line breaks in the tidied text survive.
fn chunks(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let gap_end = rest[word_end..].find(|c: char| !c.is_whitespace()).map_or(rest.len(), |k| word_end + k);
        out.push((&rest[..word_end], &rest[word_end..gap_end]));
        rest = &rest[gap_end..];
    }
    out
}
