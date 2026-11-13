//! The metrics of plan 7.3, counted per output so they can be summed over any set of cases.
//!
//! - **Term recall:** the share of term occurrences in the reference that the output has with the
//!   exact spelling ([`text::count_term`]).
//! - **WER:** word errors of the normalised output against the normalised reference
//!   ([`text::normalise`]), over the reference's words.
//! - **False corrections:** dictionary and rule edits that turn words matching the reference
//!   into ones that don't, per 1,000 output words. The edits are replayed in pipeline order, and
//!   before each one the text so far is aligned with the reference: an edit is a false correction
//!   when every word it touches was aligned to an equal reference word and the normalised words
//!   change. With `every_dictionary_edit_counts` (clips without terms, and the stress test) every
//!   dictionary edit counts.

use std::ops::{AddAssign, Range};

use anyhow::{bail, ensure};
use psst_proto::{Edit, Final, Stage};
use serde::{Deserialize, Serialize};

use crate::{align::align, text};

/// What one output, or a set of outputs, scored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Normalised words in the reference.
    pub ref_words: u64,
    /// Normalised words in the output.
    pub output_words: u64,
    /// Substitutions, deletions and insertions against the reference.
    pub word_errors: u64,
    /// Occurrences of the case's terms in the reference.
    pub term_occurrences: u64,
    /// Of those, how many the output has with the exact spelling.
    pub term_hits: u64,
    pub false_corrections: u64,
}

impl AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.ref_words += other.ref_words;
        self.output_words += other.output_words;
        self.word_errors += other.word_errors;
        self.term_occurrences += other.term_occurrences;
        self.term_hits += other.term_hits;
        self.false_corrections += other.false_corrections;
    }
}

impl Counts {
    /// Term recall in percent; `None` without term occurrences.
    pub fn term_recall(&self) -> Option<f64> {
        (self.term_occurrences > 0).then(|| 100.0 * self.term_hits as f64 / self.term_occurrences as f64)
    }

    /// Word error rate in percent; `None` without reference words.
    pub fn wer(&self) -> Option<f64> {
        (self.ref_words > 0).then(|| 100.0 * self.word_errors as f64 / self.ref_words as f64)
    }

    /// False corrections per 1,000 output words (counting at least one word).
    pub fn false_corrections_per_1000(&self) -> f64 {
        1000.0 * self.false_corrections as f64 / self.output_words.max(1) as f64
    }
}

/// What an output is scored against.
#[derive(Debug, Clone, Copy)]
pub struct Scoring<'a> {
    pub reference: &'a str,
    /// The dictionary terms in the reference; each must occur in it.
    pub terms: &'a [String],
    /// The language's filler words, removed by normalisation.
    pub fillers: &'a [String],
    /// Count every dictionary edit as a false correction: for clips without terms, and the
    /// stress test.
    pub every_dictionary_edit_counts: bool,
}

/// Scores one pipeline output.
///
/// Fails when a listed term doesn't occur in the reference, or when the output's edits, applied
/// to `raw_text` in order, don't give its `text` (the pipeline's contract, docs/protocol.md).
pub fn score(scoring: &Scoring, output: &Final) -> anyhow::Result<Counts> {
    let reference = text::normalise(scoring.reference, scoring.fillers);
    let words = text::normalise(&output.text, scoring.fillers);

    let (mut term_occurrences, mut term_hits) = (0, 0);
    let mut terms: Vec<&String> = scoring.terms.iter().collect();
    terms.sort();
    terms.dedup();
    for term in terms {
        let expected = text::count_term(scoring.reference, term);
        ensure!(expected > 0, "term `{term}` is listed but doesn't occur in the reference");
        term_occurrences += expected;
        term_hits += expected.min(text::count_term(&output.text, term));
    }

    let mut false_corrections = 0;
    let rebuilt = replay_edits(&output.raw_text, &output.edits, |edit, before, region, after| {
        let counts = edit.stage == Stage::Dictionary && scoring.every_dictionary_edit_counts
            || edit.stage != Stage::Guard && breaks_words(&reference, scoring.fillers, before, region, after);
        false_corrections += u64::from(counts);
    })?;
    ensure!(
        rebuilt == output.text,
        "the edits applied to raw_text give {rebuilt:?}, not the final text {:?}",
        output.text
    );

    Ok(Counts {
        ref_words: reference.len() as u64,
        output_words: words.len() as u64,
        word_errors: align(&reference, &words).errors as u64,
        term_occurrences: term_occurrences as u64,
        term_hits: term_hits as u64,
        false_corrections,
    })
}

/// Whether an edit turned words that matched the reference into different words.
///
/// `region` is the edited part of `before`; `after` is the whole text after the edit.
fn breaks_words(reference: &[String], fillers: &[String], before: &str, region: Range<usize>, after: &str) -> bool {
    let tokens = text::tokens(before);
    // Tokens the edit touches, including ones that merely border it, so joining or splitting
    // tokens counts as a change of words.
    let touched: Vec<usize> =
        (0..tokens.len()).filter(|&i| tokens[i].start <= region.end && tokens[i].end >= region.start).collect();
    let (Some(&first), Some(&last)) = (touched.first(), touched.last()) else {
        return false;
    };
    let words = text::normalise_tokens(before, fillers);
    let hypothesis: Vec<&String> = words.iter().map(|(_, word)| word).collect();
    let alignment = align(&reference.iter().collect::<Vec<_>>(), &hypothesis);
    let mut affected =
        words.iter().zip(&alignment.matched).filter(|((token, _), _)| touched.contains(token)).peekable();
    if affected.peek().is_none() || !affected.all(|(_, &matched)| matched) {
        return false;
    }
    let start = tokens[first].start.min(region.start);
    let end = tokens[last].end.max(region.end);
    let end_after = end + after.len() - before.len();
    text::normalise(&before[start..end], fillers) != text::normalise(&after[start..end_after], fillers)
}

/// Applies `edits` to `raw` in order, calling `visit` with each edit, the text before it, the
/// part of that text it replaces, and the text after it. Returns the final text.
///
/// Each edit replaces the current text of its raw span with `after`. An edit may cover earlier
/// edits whole, but never cut through one.
fn replay_edits(
    raw: &str,
    edits: &[Edit],
    mut visit: impl FnMut(&Edit, &str, Range<usize>, &str),
) -> anyhow::Result<String> {
    struct Piece {
        raw: Range<usize>,
        text: String,
        edited: bool,
    }

    fn split(pieces: &mut Vec<Piece>, raw: &str, at: usize) -> anyhow::Result<()> {
        let Some(i) = pieces.iter().position(|p| p.raw.start < at && at < p.raw.end) else {
            return Ok(());
        };
        if pieces[i].edited {
            bail!("an edit's span starts or ends inside an earlier edit's span");
        }
        let range = pieces[i].raw.clone();
        ensure!(raw.is_char_boundary(at), "an edit's span isn't on a character boundary");
        pieces[i] = Piece { raw: range.start..at, text: raw[range.start..at].to_owned(), edited: false };
        pieces.insert(i + 1, Piece { raw: at..range.end, text: raw[at..range.end].to_owned(), edited: false });
        Ok(())
    }

    let render = |pieces: &[Piece]| pieces.iter().map(|p| p.text.as_str()).collect::<String>();
    let mut pieces = vec![Piece { raw: 0..raw.len(), text: raw.to_owned(), edited: false }];
    for edit in edits {
        let span = edit.raw_span.clone();
        ensure!(
            span.start <= span.end && span.end <= raw.len(),
            "edit span {span:?} is outside raw_text ({} bytes)",
            raw.len()
        );
        split(&mut pieces, raw, span.start)?;
        split(&mut pieces, raw, span.end)?;

        // The pieces the edit replaces. An insertion replaces none and goes after any earlier
        // insertion at the same place; a replacement leaves insertions at its edges alone.
        let is_inside = |p: &Piece| {
            if span.is_empty() {
                false
            } else if p.raw.is_empty() {
                span.start < p.raw.start && p.raw.start < span.end
            } else {
                span.start <= p.raw.start && p.raw.end <= span.end
            }
        };
        let first = pieces
            .iter()
            .position(|p| is_inside(p) || p.raw.start > span.start || (p.raw.start == span.start && !p.raw.is_empty()))
            .unwrap_or(pieces.len());
        let count = pieces[first..].iter().take_while(|p| is_inside(p)).count();

        let before = render(&pieces);
        let offset: usize = pieces[..first].iter().map(|p| p.text.len()).sum();
        let replaced: usize = pieces[first..first + count].iter().map(|p| p.text.len()).sum();
        pieces.splice(first..first + count, [Piece { raw: span, text: edit.after.clone(), edited: true }]);
        visit(edit, &before, offset..offset + replaced, &render(&pieces));
    }
    Ok(render(&pieces))
}
