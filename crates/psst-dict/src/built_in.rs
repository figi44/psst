use std::sync::LazyLock;

use psst_config::TermSpec;

/// Pinned, so a large dictionary never pushes them out of the prompt.
static TERMS: LazyLock<Vec<TermSpec>> = LazyLock::new(|| vec![TermSpec { pin: true, ..TermSpec::new("psst") }]);

/// Every server has these under `dictionary.toml`: the file's term with the same identity replaces
/// one, the database hides or replaces one as it would a file term, and one that claims a spelling
/// another term claims gives way.
pub fn built_in_terms() -> &'static [TermSpec] {
    &TERMS
}
