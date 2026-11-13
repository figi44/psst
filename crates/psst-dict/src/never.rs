//! Never rules: a term that must never replace a span.

use psst_proto::TermId;

use crate::{Dictionary, fuzzy_key, term_id};

/// `term` never replaces a span whose [`fuzzy_key`] is `span_key`, exactly or fuzzily.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NeverRule {
    pub term: TermId,
    pub span_key: String,
}

impl NeverRule {
    /// The rule for `term` (any spelling with its identity) and `span` (as written).
    pub fn new(term: &str, span: &str) -> NeverRule {
        NeverRule { term: term_id(term), span_key: fuzzy_key(span) }
    }
}

impl Dictionary {
    /// The dictionary with `rules` as its never rules, replacing any it had. Only rules whose term
    /// is in the dictionary are kept: the others are inactive, like a rule for a hidden term.
    pub fn with_never_rules(mut self, rules: impl IntoIterator<Item = NeverRule>) -> Dictionary {
        let mut never: Vec<NeverRule> = rules
            .into_iter()
            .filter(|rule| !rule.span_key.is_empty() && self.terms.iter().any(|t| t.id == rule.term))
            .collect();
        never.sort_unstable();
        never.dedup();
        self.never = never;
        self
    }

    /// The active never rules, sorted.
    pub fn never_rules(&self) -> &[NeverRule] {
        &self.never
    }

    /// Whether a never rule forbids the term with identity `term` from replacing a span that reads
    /// `span`.
    pub fn vetoes(&self, term: &str, span: &str) -> bool {
        if self.never.is_empty() {
            return false;
        }
        let key = fuzzy_key(span);
        self.never
            .binary_search_by(|rule| (rule.term.as_str(), rule.span_key.as_str()).cmp(&(term, key.as_str())))
            .is_ok()
    }
}
