use std::ops::Range;

use caseless::Caseless;
use psst_proto::TermId;
use unicode_normalization::UnicodeNormalization;

pub(crate) fn fold(text: &str) -> String {
    text.nfc().default_case_fold().nfc().collect()
}

/// No NFC after folding, unlike [`fold`]: it has to equal what [`Folded`] makes of the same
/// spelling in an NFC transcript.
pub(crate) fn match_key(text: &str) -> String {
    text.nfc().default_case_fold().collect()
}

pub fn term_id(text: &str) -> TermId {
    fold(text)
}

/// `aho-corasick` folds only ASCII, so matching runs over this copy and maps matches back.
pub(crate) struct Folded {
    pub text: String,
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

    pub fn original_range(&self, start: usize, end: usize) -> Option<Range<usize>> {
        let at = |i: usize| if i == self.text.len() { self.original_len } else { self.origin[i] };
        let (start, end) = (at(start), at(end));
        (start != NONE && end != NONE).then_some(start..end)
    }
}
