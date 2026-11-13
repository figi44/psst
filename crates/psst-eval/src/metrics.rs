use std::ops::{AddAssign, Range};

use anyhow::{bail, ensure};
use psst_proto::{Edit, Final, Stage};
use serde::{Deserialize, Serialize};

use crate::{align::align, text};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub ref_words: u64,
    pub output_words: u64,
    pub word_errors: u64,
    pub term_occurrences: u64,
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
    pub fn term_recall(&self) -> Option<f64> {
        (self.term_occurrences > 0).then(|| 100.0 * self.term_hits as f64 / self.term_occurrences as f64)
    }

    pub fn wer(&self) -> Option<f64> {
        (self.ref_words > 0).then(|| 100.0 * self.word_errors as f64 / self.ref_words as f64)
    }

    pub fn false_corrections_per_1000(&self) -> f64 {
        1000.0 * self.false_corrections as f64 / self.output_words.max(1) as f64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Scoring<'a> {
    pub reference: &'a str,
    pub terms: &'a [String],
    pub fillers: &'a [String],
    /// For references without terms (and the stress test), where any dictionary edit is wrong.
    pub every_dictionary_edit_counts: bool,
}

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

/// Only the alignment before the edit decides which words were correct. Requiring them in order
/// among the replacing words keeps a repeated word from passing for one that was lost.
/// `region` is the edited part of `before`; `after` is the whole text after the edit.
fn breaks_words(reference: &[String], fillers: &[String], before: &str, region: Range<usize>, after: &str) -> bool {
    let tokens = text::tokens(before);
    // Bordering tokens count as touched, so joining or splitting tokens is a change of words.
    let touched: Vec<usize> =
        (0..tokens.len()).filter(|&i| tokens[i].start <= region.end && tokens[i].end >= region.start).collect();
    let (Some(&first), Some(&last)) = (touched.first(), touched.last()) else {
        return false;
    };
    let words = text::normalise_tokens(before, fillers);
    let hypothesis: Vec<&String> = words.iter().map(|(_, word)| word).collect();
    let matched = align(&reference.iter().collect::<Vec<_>>(), &hypothesis).matched;
    let correct = words
        .iter()
        .zip(matched)
        .filter(|((token, _), matched)| touched.contains(token) && matched.is_some())
        .map(|((_, word), _)| word);

    // The same tokens after the edit: only the edited region changed length.
    let start = tokens[first].start.min(region.start);
    let end = tokens[last].end.max(region.end) + after.len() - before.len();
    let new = text::normalise(&after[start..end], fillers);
    let mut rest = new.iter();
    !correct.into_iter().all(|word| rest.any(|n| n == word))
}

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

        // An insertion goes after any earlier insertion at the same place; a replacement leaves
        // insertions at its edges alone.
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
