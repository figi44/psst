//! [`PHONETIC_MAX_DISTANCE`], [`EDGE_WORD_RANK`] and the possessive rule were tuned on Common
//! Voice split B sentences outside `evals/stress/`: edit distance and Double Metaphone alone made
//! about 5 false corrections per 1,000 words without probabilities.

use std::{cmp::Reverse, collections::HashMap, ops::Range};

use rphonetic::DoubleMetaphone;
use unicode_normalization::char::is_combining_mark;

use psst_proto::TermId;

use crate::{
    Term,
    common::{common_rank, is_common_word},
    fold::term_id,
    key::fuzzy_key,
};

pub const FUZZY_LANGUAGE: &str = "en";

const MAX_SPAN_TOKENS: usize = 4;
const MIN_KEY_CHARS: usize = 4;
const LENGTH_SLACK_CHARS: usize = 2;
const LENGTH_SLACK_SHARE: f64 = 0.25;
const PHONETIC_FACTOR: f64 = 0.3;
/// Double Metaphone ignores vowels and doubled letters, so without this cap "Thales" sounds like
/// Talos, and "pepper" like Piper.
const PHONETIC_MAX_DISTANCE: f64 = 1.0 / 3.0;
/// Longer spans never start or end with one of this many most common words: "to go" sounds like
/// Tokio and "well and" like Wayland, and a term is rarely misheard as a function word.
const EDGE_WORD_RANK: usize = 100;
const AMBIGUITY: f64 = 0.02;
const EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    High,
    Medium,
    Low,
}

impl Band {
    fn of(prob: Option<f32>) -> Band {
        match prob {
            Some(p) if p >= 0.85 => Band::High,
            Some(p) if p < 0.50 => Band::Low,
            _ => Band::Medium,
        }
    }

    fn threshold(self) -> f64 {
        match self {
            Band::High => 0.08,
            Band::Medium => 0.18,
            Band::Low => 0.25,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyToken {
    /// Tokens are in text order, with only whitespace between neighbours.
    pub range: Range<usize>,
    /// The lowest probability of the engine words it overlaps.
    pub prob: Option<f32>,
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch<'d> {
    pub tokens: Range<usize>,
    /// Leaves out the span's surrounding punctuation and any suffix taken off to match, which stay
    /// in the text.
    pub range: Range<usize>,
    pub term: &'d Term,
    /// Lower is closer.
    pub score: f32,
}

/// Without Double Metaphone's usual limit of 4 characters: the whole word has to sound alike.
fn phonetic_codes(key: &[char]) -> Option<(String, Option<String>)> {
    if !key.iter().all(char::is_ascii_lowercase) {
        return None;
    }
    let word: String = key.iter().collect();
    let result = DoubleMetaphone::new(None).double_metaphone(&word);
    let (primary, alternate) = (result.primary(), result.alternate());
    if primary.is_empty() {
        return None;
    }
    let alternate = (!alternate.is_empty() && alternate != primary).then_some(alternate);
    Some((primary, alternate))
}

#[derive(Debug, Clone)]
struct Entry {
    term: usize,
    key: Vec<char>,
    slack: usize,
    possessives: usize,
    common: Option<TermId>,
}

#[derive(Debug, Clone, Default)]
struct Bucket {
    entries: Vec<usize>,
    masks: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Index {
    entries: Vec<Entry>,
    by_len: Vec<Bucket>,
    by_key: HashMap<String, Vec<usize>>,
    by_code: HashMap<String, Vec<usize>>,
    reachable: Vec<bool>,
    any_common: bool,
}

impl Index {
    pub fn new(terms: &[Term]) -> Self {
        let mut index = Index::default();
        for (i, term) in terms.iter().enumerate() {
            if !term.fuzzy || !term.languages.includes(FUZZY_LANGUAGE) {
                continue;
            }
            let key_text = fuzzy_key(&term.text);
            let key: Vec<char> = key_text.chars().collect();
            if key.is_empty() {
                continue;
            }
            let e = index.entries.len();
            if index.by_len.len() <= key.len() {
                index.by_len.resize(key.len() + 1, Bucket::default());
            }
            let bucket = &mut index.by_len[key.len()];
            bucket.entries.push(e);
            bucket.masks.push(mask(&key));
            if let Some((primary, alternate)) = phonetic_codes(&key) {
                for code in std::iter::once(primary).chain(alternate) {
                    index.by_code.entry(code).or_default().push(e);
                }
            }
            index.by_key.entry(key_text).or_default().push(e);
            let words: Vec<&str> = term.text.split_whitespace().collect();
            let possessives = words[..words.len().saturating_sub(1)].iter().filter(|w| ends_possessive(w)).count();
            let slack = LENGTH_SLACK_CHARS.max((LENGTH_SLACK_SHARE * key.len() as f64 + EPSILON).floor() as usize);
            let common = is_common_word(FUZZY_LANGUAGE, &term.text).then(|| term.id.clone());
            index.any_common |= common.is_some();
            index.entries.push(Entry { term: i, key, slack, possessives, common });
        }
        let longest = index.entries.iter().map(|e| e.key.len() + e.slack).max().unwrap_or(0);
        index.reachable =
            (0..=longest).map(|len| index.entries.iter().any(|e| len.abs_diff(e.key.len()) <= e.slack)).collect();
        index
    }

    pub fn find<'d>(
        &self,
        terms: &'d [Term],
        text: &str,
        tokens: &[FuzzyToken],
        vetoes: impl Fn(&Term, &str) -> bool,
    ) -> Vec<FuzzyMatch<'d>> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let words: Vec<Option<Word>> =
            tokens.iter().map(|t| t.available.then(|| Word::of(text, t.range.clone())).flatten()).collect();
        let mut scratch = Scratch { seen: vec![0; self.entries.len()], ..Scratch::default() };
        let mut candidates: Vec<Candidate> = Vec::new();

        for start in 0..tokens.len() {
            let Some(first) = &words[start] else { continue };
            let most = if first.function_word { 1 } else { MAX_SPAN_TOKENS };
            let mut prob: Option<f32> = None;
            let mut possessives = 0;
            let mut key_len = 0;
            for end in start + 1..=(start + most).min(tokens.len()) {
                let last = end - 1;
                let Some(word) = &words[last] else { break };
                // A span with a disallowed character can't be extended to drop it.
                let allowed = match last - start {
                    0 => first.core_allowed,
                    1 => first.head_allowed && word.tail_allowed,
                    _ => words[last - 1].as_ref().is_some_and(|w| w.all_allowed) && word.tail_allowed,
                };
                // A suffix form is at most two characters shorter, and a longer span only longer.
                key_len += word.key.len();
                if !allowed || key_len.saturating_sub(2) >= self.reachable.len() {
                    break;
                }
                if let Some(p) = tokens[last].prob {
                    prob = Some(prob.map_or(p, |q| q.min(p)));
                }
                if last > start {
                    possessives += usize::from(words[last - 1].as_ref().is_some_and(|w| w.possessive));
                    if word.function_word {
                        continue;
                    }
                }
                let span = Span { tokens: start..end, band: Band::of(prob), possessives };
                self.score_span(text, &words, &span, &mut scratch, &mut candidates);
            }
        }

        candidates.sort_by(|a, b| {
            a.score
                .total_cmp(&b.score)
                .then_with(|| Reverse(a.tokens.len()).cmp(&Reverse(b.tokens.len())))
                .then_with(|| a.tokens.start.cmp(&b.tokens.start))
                .then_with(|| a.term.cmp(&b.term))
        });
        let mut taken = vec![false; tokens.len()];
        let mut matches = Vec::new();
        for candidate in candidates {
            if taken[candidate.tokens.clone()].iter().any(|&t| t) {
                continue;
            }
            let term = &terms[candidate.term];
            // A rule may name the span as written, or without the suffix the match keeps.
            if vetoes(term, &text[candidate.range.start..candidate.written_end])
                || vetoes(term, &text[candidate.range.clone()])
            {
                continue;
            }
            taken[candidate.tokens.clone()].iter_mut().for_each(|t| *t = true);
            matches.push(FuzzyMatch {
                tokens: candidate.tokens,
                range: candidate.range,
                term,
                score: candidate.score as f32,
            });
        }
        matches.sort_by_key(|m| m.tokens.start);
        matches
    }

    fn score_span(
        &self,
        text: &str,
        words: &[Option<Word>],
        span: &Span,
        scratch: &mut Scratch,
        candidates: &mut Vec<Candidate>,
    ) {
        let threshold = span.band.threshold();
        let span_words: Vec<&Word> = words[span.tokens.clone()].iter().flatten().collect();
        let (head, last) = span_words.split_at(span_words.len() - 1);
        let last = last[0];
        let held_back = |common: bool| head.is_empty() && common && span.band != Band::Low;
        if held_back(last.common) {
            return;
        }
        let start = head.first().unwrap_or(&last).core.start;
        let head_len: usize = head.iter().map(|w| w.key.len()).sum();
        let head_mask = head.iter().fold(0, |m, w| m | w.mask);

        scratch.best.clear();
        let as_written = Stem { end: last.core.end, key: last.key.clone(), mask: last.mask, common: last.common };
        let forms = || std::iter::once(&as_written).chain(&last.stems);
        // As in exact matching, a term that is a common word never rewrites a single word that is
        // it in some form: "games" would become "Gamess", and "home's" "Home".
        let identities: Vec<TermId> = match head.is_empty() && self.any_common {
            true => forms().map(|stem| term_id(&text[start..stem.end])).collect(),
            false => Vec::new(),
        };
        for stem in forms() {
            let len = head_len + stem.key.len();
            if held_back(stem.common) || len < MIN_KEY_CHARS || !self.reachable.get(len).is_some_and(|&r| r) {
                continue;
            }
            scratch.key.clear();
            head.iter().for_each(|w| scratch.key.extend_from_slice(&w.key));
            scratch.key.extend_from_slice(&stem.key);
            let form = Form {
                mask: head_mask | stem.mask,
                threshold,
                possessives: span.possessives,
                end: stem.end,
                identities: &identities,
            };
            self.score_form(&form, scratch);
        }

        let best = &mut scratch.best;
        best.sort_by(|a, b| a.score.total_cmp(&b.score).then_with(|| a.entry.cmp(&b.entry)));
        for (i, found) in best.iter().enumerate() {
            if best.get(i + 1).is_some_and(|next| next.score - found.score <= AMBIGUITY + EPSILON) {
                break;
            }
            candidates.push(Candidate {
                tokens: span.tokens.clone(),
                range: start..found.end,
                written_end: last.core.end,
                term: self.entries[found.entry].term,
                score: found.score,
            });
        }
    }

    fn score_form(&self, form: &Form, scratch: &mut Scratch) {
        let Scratch { key, key_text, row, best, seen, form: stamp } = scratch;
        *stamp += 1;
        let stamp = *stamp;
        let len = key.len();
        let threshold = form.threshold;
        let mut consider = |e: usize, phonetic: bool| {
            // Phonetic entries come first, so one seen already had the larger allowance.
            if seen[e] == stamp {
                return;
            }
            seen[e] = stamp;
            let entry = &self.entries[e];
            let term_len = entry.key.len();
            if len.abs_diff(term_len) > entry.slack
                || form.possessives > entry.possessives
                || entry.common.as_ref().is_some_and(|id| form.identities.contains(id))
            {
                return;
            }
            let longest = len.max(term_len) as f64;
            let share = match phonetic {
                true => (threshold / PHONETIC_FACTOR).min(PHONETIC_MAX_DISTANCE),
                false => threshold,
            };
            let limit = (share * longest + EPSILON).floor() as usize;
            let Some(distance) = levenshtein_within(key, &entry.key, limit, row) else { return };
            let mut score = distance as f64 / longest;
            if phonetic {
                score *= PHONETIC_FACTOR;
            }
            if score >= threshold - EPSILON {
                return;
            }
            match best.iter_mut().find(|b| b.entry == e) {
                Some(found) if score < found.score => (found.score, found.end) = (score, form.end),
                Some(_) => {}
                None => best.push(Best { entry: e, score, end: form.end }),
            }
        };

        if !self.by_code.is_empty()
            && let Some((primary, alternate)) = phonetic_codes(key)
        {
            for code in std::iter::once(primary).chain(alternate) {
                for &e in self.by_code.get(&code).into_iter().flatten() {
                    consider(e, true);
                }
            }
        }
        key_text.clear();
        key_text.extend(key.iter());
        for &e in self.by_key.get(key_text.as_str()).into_iter().flatten() {
            consider(e, false);
        }
        // Exact keys came from `by_key`; everything else takes at least one edit.
        for (term_len, bucket) in self.by_len.iter().enumerate() {
            let limit = (threshold * len.max(term_len) as f64 + EPSILON).floor() as usize;
            if limit == 0 || len.abs_diff(term_len) > limit {
                continue;
            }
            for (&e, &term_mask) in bucket.entries.iter().zip(&bucket.masks) {
                if missing(form.mask, term_mask) <= limit {
                    consider(e, false);
                }
            }
        }
    }
}

#[derive(Default)]
struct Scratch {
    key: Vec<char>,
    key_text: String,
    row: Vec<usize>,
    best: Vec<Best>,
    seen: Vec<u32>,
    form: u32,
}

struct Best {
    entry: usize,
    score: f64,
    end: usize,
}

struct Span {
    tokens: Range<usize>,
    band: Band,
    /// "claude's code" is someone's code, not Claude Code: only a term with as many `'s` words
    /// before its last can match.
    possessives: usize,
}

struct Form<'a> {
    mask: u64,
    threshold: f64,
    possessives: usize,
    end: usize,
    identities: &'a [TermId],
}

struct Candidate {
    tokens: Range<usize>,
    range: Range<usize>,
    written_end: usize,
    term: usize,
    score: f64,
}

struct Word {
    core: Range<usize>,
    core_allowed: bool,
    head_allowed: bool,
    tail_allowed: bool,
    all_allowed: bool,
    key: Vec<char>,
    mask: u64,
    common: bool,
    function_word: bool,
    possessive: bool,
    stems: Vec<Stem>,
}

struct Stem {
    end: usize,
    key: Vec<char>,
    mask: u64,
    common: bool,
}

impl Word {
    fn of(text: &str, range: Range<usize>) -> Option<Word> {
        let token = &text[range.clone()];
        let start = token.find(char::is_alphanumeric)?;
        let end =
            token.rfind(char::is_alphanumeric).map(|i| i + token[i..].chars().next().map_or(1, char::len_utf8))?;
        let allowed = |part: &str| part.chars().all(is_span_char);
        let core = &token[start..end];
        let core_start = range.start + start;
        let key = key_chars(core);
        let core_rank = rank(core, &key);
        let stems = suffixes(core)
            .into_iter()
            .map(|stem_len| {
                let stem = &core[..stem_len];
                let key = key_chars(stem);
                Stem { end: core_start + stem_len, mask: mask(&key), common: rank(stem, &key).is_some(), key }
            })
            .collect();
        Some(Word {
            core: core_start..range.start + end,
            core_allowed: allowed(core),
            head_allowed: allowed(&token[start..]),
            tail_allowed: allowed(&token[..end]),
            all_allowed: allowed(token),
            mask: mask(&key),
            key,
            common: core_rank.is_some(),
            function_word: core_rank.is_some_and(|rank| rank <= EDGE_WORD_RANK),
            possessive: ends_possessive(core),
            stems,
        })
    }
}

fn rank(word: &str, key: &[char]) -> Option<usize> {
    let own = common_rank(FUZZY_LANGUAGE, word);
    let key: String = key.iter().collect();
    let by_key = if key.eq_ignore_ascii_case(word) { None } else { common_rank(FUZZY_LANGUAGE, &key) };
    own.into_iter().chain(by_key).min()
}

/// [`fuzzy_key`] as characters; the ASCII shortcut must give the same result.
fn key_chars(text: &str) -> Vec<char> {
    if text.is_ascii() {
        text.bytes().filter(u8::is_ascii_alphanumeric).map(|b| char::from(b.to_ascii_lowercase())).collect()
    } else {
        fuzzy_key(text).chars().collect()
    }
}

/// `s'` is the `s` form: its apostrophe is the token's trailing punctuation.
fn suffixes(word: &str) -> Vec<usize> {
    let mut chars = word.chars().rev();
    let (Some('s' | 'S'), before) = (chars.next(), chars.next()) else { return Vec::new() };
    let mut lengths = Vec::new();
    match before {
        Some(c) if is_apostrophe(c) => lengths.push(word.len() - 1 - c.len_utf8()),
        Some('e' | 'E') => lengths.push(word.len() - 2),
        _ => {}
    }
    lengths.push(word.len() - 1);
    lengths.retain(|&len| len > 0 && word[..len].chars().next_back().is_some_and(char::is_alphanumeric));
    lengths
}

/// No spaces: they only ever come between tokens.
fn is_span_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c) || matches!(c, '-' | '\u{2010}' | '\u{2011}') || is_apostrophe(c)
}

fn ends_possessive(word: &str) -> bool {
    let mut chars = word.chars().rev();
    matches!(chars.next(), Some('s' | 'S')) && chars.next().is_some_and(is_apostrophe)
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '\u{02bc}')
}

fn mask(key: &[char]) -> u64 {
    key.iter().fold(0, |mask, &c| {
        let bit = match c {
            'a'..='z' => c as u32 - 'a' as u32,
            '0'..='9' => 26 + c as u32 - '0' as u32,
            _ => 36 + c as u32 % 28,
        };
        mask | 1 << bit
    })
}

/// A lower bound on the edit distance: every class one key has and the other lacks takes an edit.
fn missing(a: u64, b: u64) -> usize {
    (a & !b).count_ones().max((b & !a).count_ones()) as usize
}

fn levenshtein_within(a: &[char], b: &[char], limit: usize, row: &mut Vec<usize>) -> Option<usize> {
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    let over = limit + 1;
    row.clear();
    row.extend((0..=b.len()).map(|j| j.min(over)));
    for (i, &ca) in a.iter().enumerate() {
        let i = i + 1;
        let lo = i.saturating_sub(limit).max(1);
        let hi = (i + limit).min(b.len());
        // Left of the band: the first column, or else counted as over the limit.
        let mut diagonal = row[lo - 1];
        row[lo - 1] = if lo == 1 { i.min(over) } else { over };
        let mut lowest = row[lo - 1];
        for j in lo..=hi {
            let above = row[j];
            let value = (diagonal + usize::from(ca != b[j - 1])).min(above + 1).min(row[j - 1] + 1).min(over);
            diagonal = above;
            row[j] = value;
            lowest = lowest.min(value);
        }
        if lowest > limit {
            return None;
        }
    }
    let distance = row[b.len()];
    (distance <= limit).then_some(distance)
}
