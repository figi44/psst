use psst_proto::TermId;

use crate::{Dictionary, fuzzy_key, term_id};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NeverRule {
    pub term: TermId,
    pub span_key: String,
}

impl NeverRule {
    pub fn new(term: &str, span: &str) -> NeverRule {
        NeverRule { term: term_id(term), span_key: fuzzy_key(span) }
    }
}

impl Dictionary {
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

    pub fn never_rules(&self) -> &[NeverRule] {
        &self.never
    }

    /// `term` is an identity ([`term_id`]), not a spelling.
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
