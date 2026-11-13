use std::collections::{BTreeMap, HashMap, HashSet};

use psst_config::{DictionaryFile, TermSpec};
use psst_proto::TermId;

use crate::{Dictionary, Error, NeverRule, Result, term_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerEntry {
    Set(TermSpec),
    Hide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerTerm {
    pub entry: LayerEntry,
    /// Any increasing order: of two conflicting entries, the later one is left out.
    pub saved: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub terms: BTreeMap<TermId, LayerTerm>,
    pub never: Vec<NeverRule>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub dictionary: Dictionary,
    /// Database entries left out, and why.
    pub skipped: BTreeMap<TermId, String>,
}

impl Dictionary {
    pub fn resolve(file: &DictionaryFile, layer: &Layer) -> Result<Resolved> {
        Dictionary::from_file(file)?;
        let mut skipped = BTreeMap::new();
        for (id, entry) in &layer.terms {
            if let LayerEntry::Set(spec) = &entry.entry
                && term_id(&spec.text) != *id
            {
                skipped.insert(
                    id.clone(),
                    format!("the entry for `{id}` holds `{}`, which has another identity", spec.text),
                );
            }
        }
        loop {
            let (specs, from_layer) = compose(file, layer, &skipped);
            let err = match Dictionary::from_specs(specs) {
                Ok(dictionary) => {
                    let dictionary = dictionary.with_never_rules(layer.never.iter().cloned());
                    return Ok(Resolved { dictionary, skipped });
                }
                Err(err) => err,
            };
            let Error::SpellingConflict { first, second, .. } = &err else { return Err(err) };
            // The file is valid alone and hiding only removes terms, so at least one side is the
            // database's, and it gives way: the file always wins. Saves are checked, so this
            // happens only when the file changed under the database.
            let loser = [term_id(first), term_id(second)]
                .into_iter()
                .filter_map(|id| from_layer.get(&id).map(|&saved| (saved, id)))
                .max()
                .map(|(_, id)| id);
            match loser {
                Some(id) => skipped.insert(id, err.to_string()),
                None => return Err(err),
            };
        }
    }
}

/// Deterministic: the file's terms in its order, then the database-only terms by identity.
fn compose<'a>(
    file: &'a DictionaryFile,
    layer: &'a Layer,
    skipped: &BTreeMap<TermId, String>,
) -> (Vec<&'a TermSpec>, HashMap<TermId, u64>) {
    let entry = |id: &TermId| layer.terms.get(id).filter(|_| !skipped.contains_key(id));
    let mut specs = Vec::with_capacity(file.terms.len() + layer.terms.len());
    let mut from_layer = HashMap::new();
    let mut in_file = HashSet::with_capacity(file.terms.len());
    for spec in &file.terms {
        let id = term_id(&spec.text);
        match entry(&id) {
            Some(LayerTerm { entry: LayerEntry::Hide, .. }) => {}
            Some(LayerTerm { entry: LayerEntry::Set(replacement), saved }) => {
                specs.push(replacement);
                from_layer.insert(id.clone(), *saved);
            }
            None => specs.push(spec),
        }
        in_file.insert(id);
    }
    for (id, term) in &layer.terms {
        if let LayerEntry::Set(spec) = &term.entry
            && !in_file.contains(id)
            && !skipped.contains_key(id)
        {
            specs.push(spec);
            from_layer.insert(id.clone(), term.saved);
        }
    }
    (specs, from_layer)
}
