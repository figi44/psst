use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

pub fn fuzzy_key(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}
