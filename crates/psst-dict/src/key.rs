//! The fuzzy match key.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// A spelling's fuzzy match key: NFKD with the combining marks dropped, so diacritics
/// fold, in lowercase, keeping only letters and digits. "Cube Nettie" and "cube-nettie" are both
/// `cubenettie`, and "Dvořák" is `dvorak`. Several terms may share a key. Never rules compare spans
/// by it.
pub fn fuzzy_key(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}
