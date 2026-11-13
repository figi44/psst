//! Fuzzy matching (plan 7.2 steps 3–7): spans of up to four tokens in an English session, scored
//! against the match keys of the `fuzzy` terms by edit distance and Double Metaphone.
//!
//! Three rules go beyond the plan's initial values. They were tuned on Common Voice split B
//! sentences outside `evals/stress/`, where the plan's rules alone made about 5 false corrections
//! per 1,000 words without probabilities:
//!
//! - sounding alike only helps a span whose letters are already close ([`PHONETIC_MAX_DISTANCE`]);
//! - a span of several words never starts or ends with one of the most common words
//!   ([`EDGE_WORD_RANK`]);
//! - a span with a possessive before its last word only matches a term that has one there too.

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

/// The only language fuzzy matching runs in.
pub const FUZZY_LANGUAGE: &str = "en";

/// Most tokens a span may have.
const MAX_SPAN_TOKENS: usize = 4;
/// Shortest span key, in characters.
const MIN_KEY_CHARS: usize = 4;
/// A span key may differ in length from the term key by the larger of this many characters and
/// [`LENGTH_SLACK_SHARE`] of the term key's length.
const LENGTH_SLACK_CHARS: usize = 2;
const LENGTH_SLACK_SHARE: f64 = 0.25;
/// A span that sounds like the term (a Double Metaphone code in common) has its score multiplied
/// by this…
const PHONETIC_FACTOR: f64 = 0.3;
/// …as long as its edit distance is at most this share of the longer key. The codes ignore vowels
/// and doubled letters, so without the limit they match where half the letters differ: "Thales"
/// sounds like Talos, and "pepper" like Piper.
const PHONETIC_MAX_DISTANCE: f64 = 1.0 / 3.0;
/// A span of several tokens never starts or ends with one of this many most common English words.
/// Function words next to an ordinary word sound like many short terms ("to go" → Tokio, "well
/// and" → Wayland), and a term is rarely misheard as one.
const EDGE_WORD_RANK: usize = 100;
/// The two best terms scoring within this of each other for one span leave it alone.
const AMBIGUITY: f64 = 0.02;
/// Slack for comparing scores computed in floating point.
const EPSILON: f64 = 1e-9;

/// How sure the engine was of a span: the lowest `prob` of its tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    /// `prob` ≥ 0.85.
    High,
    /// 0.50 ≤ `prob` < 0.85, or no `prob` at all.
    Medium,
    /// `prob` < 0.50.
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

    /// A span is replaced only when it scores below this.
    fn threshold(self) -> f64 {
        match self {
            Band::High => 0.08,
            Band::Medium => 0.18,
            Band::Low => 0.25,
        }
    }
}

/// A token of the text, as fuzzy matching sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyToken {
    /// Byte range of the token in the text: a run of non-whitespace. Tokens are in text order,
    /// with only whitespace between neighbours.
    pub range: Range<usize>,
    /// The lowest probability the engine gave the words it overlaps, if it gave any.
    pub prob: Option<f32>,
    /// Whether the token may be part of a match. Protected and removed tokens may not.
    pub available: bool,
}

/// One fuzzy match in a text.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch<'d> {
    /// Indices of the tokens it spans.
    pub tokens: Range<usize>,
    /// Byte range of the characters to replace with the term's `text`: the span without its
    /// leading and trailing punctuation, and without the suffix a suffix form put back.
    pub range: Range<usize>,
    pub term: &'d Term,
    /// `levenshtein / max length`, times 0.3 when the span sounds like the term. Lower is closer.
    pub score: f32,
}

/// The Double Metaphone codes of a key of ASCII letters, primary first, without the usual limit
/// of 4 characters: the whole word has to sound alike. `None` for a key with anything else, such
/// as digits, which never sounds like a term.
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

/// A term's key, ready for matching.
#[derive(Debug, Clone)]
struct Entry {
    term: usize,
    key: Vec<char>,
    /// How much a span key's length may differ from this key's.
    slack: usize,
    /// Words of the term's `text` that end in `'s`, apart from its last word.
    possessives: usize,
    /// The term's identity, when its `text` is a common English word. A single word with that
    /// identity is left as it is, as the exact spellings leave it: a term `Home` doesn't recase
    /// "home".
    common: Option<TermId>,
}

/// The entries with one key length, with their [`mask`]s side by side for a quick scan.
#[derive(Debug, Clone, Default)]
struct Bucket {
    entries: Vec<usize>,
    masks: Vec<u64>,
}

/// The fuzzy terms of a dictionary. Built once per dictionary.
#[derive(Debug, Clone, Default)]
pub(crate) struct Index {
    entries: Vec<Entry>,
    /// Entries by key length in characters.
    by_len: Vec<Bucket>,
    /// Entries by key, for spans that match a key exactly.
    by_key: HashMap<String, Vec<usize>>,
    /// Entries by each of their Double Metaphone codes.
    by_code: HashMap<String, Vec<usize>>,
    /// Whether a span key of each length is close enough in length to some entry's.
    reachable: Vec<bool>,
    /// Whether any entry's term is a common word.
    any_common: bool,
}

impl Index {
    /// Indexes the terms that take part in fuzzy matching: `fuzzy` ones valid in English.
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

    /// The fuzzy matches among `tokens` of `text`, in text order and never overlapping. `vetoes`
    /// says whether a never rule forbids a term replacing a span's text.
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
            // No longer span starts with a function word.
            let most = if first.function_word { 1 } else { MAX_SPAN_TOKENS };
            let mut prob: Option<f32> = None;
            let mut possessives = 0;
            let mut key_len = 0;
            for end in start + 1..=(start + most).min(tokens.len()) {
                let last = end - 1;
                let Some(word) = &words[last] else { break };
                // Only letters, digits, spaces, hyphens and apostrophes between the span's first and
                // last letter or digit. A span that fails can't be extended either.
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

    /// Scores one span, as written and in its suffix forms, against every term. Keeps a candidate
    /// for each term it scores under its band's threshold, best first, for as long as each is
    /// clearly better than the next: two terms too close to call leave the span to neither, and
    /// to none after them.
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
        // A single common word is rewritten only when the engine wasn't sure of it.
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
        // A single word that is, in any of its forms, a term that is a common word is never
        // changed to that term: "games" doesn't become "Gamess", nor "home's" "Home".
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

    /// Scores one form of a span, whose key is in `scratch.key`, against the entries that can
    /// score under the threshold: those that sound like it, those with exactly its key, and those
    /// few enough edits away. Keeps each entry's best score in `scratch.best`.
    fn score_form(&self, form: &Form, scratch: &mut Scratch) {
        let Scratch { key, key_text, row, best, seen, form: stamp } = scratch;
        *stamp += 1;
        let stamp = *stamp;
        let len = key.len();
        let threshold = form.threshold;
        let mut consider = |e: usize, phonetic: bool| {
            // An entry that sounds alike was scored with the larger allowance already.
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
            // The most edits that can still score under the threshold.
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
        // Everything else takes at least one edit, and few enough to stay under the threshold.
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

/// Buffers reused while scoring the spans of one text.
#[derive(Default)]
struct Scratch {
    /// The key of the form being scored.
    key: Vec<char>,
    key_text: String,
    /// A row of the edit distance table.
    row: Vec<usize>,
    /// The best score per entry for the span being scored.
    best: Vec<Best>,
    /// Per entry, the last form that scored it.
    seen: Vec<u32>,
    /// Counts the forms scored.
    form: u32,
}

/// An entry's best score for a span, and where that form's replacement ends.
struct Best {
    entry: usize,
    score: f64,
    end: usize,
}

/// A span of tokens that passed the character restriction.
struct Span {
    tokens: Range<usize>,
    band: Band,
    /// Tokens before the last that end in `'s`: "claude's code" is someone's code, not Claude
    /// Code. Only a term with as many such words can match.
    possessives: usize,
}

/// One form of a span: as written, or with a suffix taken off.
struct Form<'a> {
    mask: u64,
    threshold: f64,
    possessives: usize,
    /// Where the replaced characters end.
    end: usize,
    /// For a single word, the identity of each of its forms.
    identities: &'a [TermId],
}

/// A span and a term it scored under its threshold.
struct Candidate {
    tokens: Range<usize>,
    /// The characters the term replaces.
    range: Range<usize>,
    /// Where the span's last letter or digit ends, suffix and all.
    written_end: usize,
    term: usize,
    score: f64,
}

/// What fuzzy matching needs to know about an available token, worked out once per text.
struct Word {
    /// Byte range in the text from its first letter or digit to the end of its last.
    core: Range<usize>,
    /// The core holds only characters a span may contain.
    core_allowed: bool,
    /// From the core's start to the token's end, for the first token of a longer span.
    head_allowed: bool,
    /// From the token's start to the core's end, for the last token of a longer span.
    tail_allowed: bool,
    /// The whole token, for a token in the middle.
    all_allowed: bool,
    key: Vec<char>,
    mask: u64,
    /// One of the most common English words, as written or by its key ("system's" is
    /// "systems").
    common: bool,
    /// One of the [`EDGE_WORD_RANK`] most common, which never start or end a longer span.
    function_word: bool,
    /// Ends in `'s`.
    possessive: bool,
    /// The token as the last of a span with an English suffix taken off.
    stems: Vec<Stem>,
}

/// The end of a span's last token in one of its forms.
struct Stem {
    /// Where the replaced characters end.
    end: usize,
    key: Vec<char>,
    mask: u64,
    common: bool,
}

impl Word {
    /// `None` for a token without letters or digits, which is never part of a span.
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

/// A word's rank among the most common English words: the better of its own and its key's.
fn rank(word: &str, key: &[char]) -> Option<usize> {
    let own = common_rank(FUZZY_LANGUAGE, word);
    let key: String = key.iter().collect();
    let by_key = if key.eq_ignore_ascii_case(word) { None } else { common_rank(FUZZY_LANGUAGE, &key) };
    own.into_iter().chain(by_key).min()
}

/// [`fuzzy_key`] as characters, without Unicode normalisation for ASCII text.
fn key_chars(text: &str) -> Vec<char> {
    if text.is_ascii() {
        text.bytes().filter(u8::is_ascii_alphanumeric).map(|b| char::from(b.to_ascii_lowercase())).collect()
    } else {
        fuzzy_key(text).chars().collect()
    }
}

/// The lengths of `word` with an English suffix taken off (`'s`, `es`, `s`), where what's left
/// still ends in a letter or digit. `s'` is the `s` form, since the apostrophe after it is the
/// token's trailing punctuation.
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

/// Letters, digits, hyphens and apostrophes (spaces only ever come between tokens).
fn is_span_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c) || matches!(c, '-' | '\u{2010}' | '\u{2011}') || is_apostrophe(c)
}

/// Whether a word ends in `'s` (or `’s`).
fn ends_possessive(word: &str) -> bool {
    let mut chars = word.chars().rev();
    matches!(chars.next(), Some('s' | 'S')) && chars.next().is_some_and(is_apostrophe)
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '\u{02bc}')
}

/// A bit per character class: `a`–`z`, `0`–`9`, and the rest hashed into the remaining bits.
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

/// A lower bound on the edit distance from the classes present in each key: every class one key
/// has and the other lacks takes at least one edit.
fn missing(a: u64, b: u64) -> usize {
    (a & !b).count_ones().max((b & !a).count_ones()) as usize
}

/// The Levenshtein distance between `a` and `b` if it's at most `limit`. Only the cells within
/// `limit` of the diagonal can hold such a distance, so only those are computed.
fn levenshtein_within(a: &[char], b: &[char], limit: usize, row: &mut Vec<usize>) -> Option<usize> {
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    // Anything over the limit is as good as `over`.
    let over = limit + 1;
    row.clear();
    row.extend((0..=b.len()).map(|j| j.min(over)));
    for (i, &ca) in a.iter().enumerate() {
        let i = i + 1;
        let lo = i.saturating_sub(limit).max(1);
        let hi = (i + limit).min(b.len());
        // The cell left of the band: the first column, or outside the band.
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
