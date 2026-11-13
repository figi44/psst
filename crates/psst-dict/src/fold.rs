//! Unicode case folding, for term identity and for case-insensitive matching.

use std::ops::Range;

use caseless::Caseless;
use psst_proto::TermId;
use unicode_normalization::UnicodeNormalization;

/// NFC, full Unicode case folding, then NFC again.
pub(crate) fn fold(text: &str) -> String {
    text.nfc().default_case_fold().nfc().collect()
}

/// A term's identity key (plan 7.1): its `text` after NFC and case folding. Punctuation and
/// diacritics are kept, so `C`, `C++` and `C#` are three terms, and `GPU` and `gpu` are one.
pub fn term_id(text: &str) -> TermId {
    fold(text)
}

/// A case-folded copy of a text that remembers where each of its characters came from.
///
/// `aho-corasick` folds only ASCII, so matching runs over this copy and maps matches back.
pub(crate) struct Folded {
    pub text: String,
    /// For each byte of `text`: the offset in the original of the character whose folding starts
    /// there, or `NONE` inside a folding.
    origin: Vec<usize>,
    original_len: usize,
}

const NONE: usize = usize::MAX;

impl Folded {
    pub fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut origin = Vec::with_capacity(original.len());
        for (offset, c) in original.char_indices() {
            if c.is_ascii() {
                text.push(c.to_ascii_lowercase());
            } else {
                text.extend(std::iter::once(c).default_case_fold());
            }
            origin.push(offset);
            origin.resize(text.len(), NONE);
        }
        Self { text, origin, original_len: original.len() }
    }

    /// The original byte range of the folded range `start..end`, unless it starts or ends inside
    /// the folding of one original character.
    pub fn original_range(&self, start: usize, end: usize) -> Option<Range<usize>> {
        let at = |i: usize| if i == self.text.len() { self.original_len } else { self.origin[i] };
        let (start, end) = (at(start), at(end));
        (start != NONE && end != NONE).then_some(start..end)
    }
}
