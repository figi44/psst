//! The database layer on top of `dictionary.toml` (plan 7.1 "Layers"), and how the two resolve
//! into the dictionary sessions use.
//!
//! The database can **add** a term, **replace** a file term with the same identity, or **hide**
//! one, and it holds the never rules. `reset` deletes the database's entry for an identity, which
//! is the [`Layer`] without it. Renaming is a hide plus an add.
//!
//! [`Dictionary::resolve`] is deterministic:
//!
//! 1. The file must be valid on its own ([`Dictionary::from_file`]); otherwise resolution fails,
//!    and the caller keeps the dictionary it has.
//! 2. The terms are the file's, in its order, each replaced by the database's version or left out
//!    when hidden, followed by the terms only the database has, by identity. Hiding a term the file
//!    doesn't have changes nothing.
//! 3. If two of them claim one spelling in a shared language, the database entry involved is left
//!    out (of two entries, the one saved later), and step 2 runs again without it, so a replaced
//!    term falls back to the file's version. The file always wins. Since every save is checked
//!    first, this happens only when the file changes under the database.
//! 4. Never rules whose term is in the result are active; the others are kept, but inactive.

use std::collections::{BTreeMap, HashMap, HashSet};

use psst_config::{DictionaryFile, TermSpec};
use psst_proto::TermId;

use crate::{Dictionary, Error, NeverRule, Result, term_id};

/// What the database holds for one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerEntry {
    /// The term, which adds it, or replaces the file's term with its identity.
    Set(TermSpec),
    /// Hides the file's term with this identity.
    Hide,
}

/// One identity's database entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerTerm {
    pub entry: LayerEntry,
    /// When it was saved, in any increasing order: of two conflicting entries, the later one is
    /// left out.
    pub saved: u64,
}

/// The database layer: entries by identity, and the never rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub terms: BTreeMap<TermId, LayerTerm>,
    pub never: Vec<NeverRule>,
}

/// The dictionary that a file and a layer give.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub dictionary: Dictionary,
    /// Database entries left out, with the reason: a conflict with the file or with an older
    /// entry, or an entry whose term has another identity than its key.
    pub skipped: BTreeMap<TermId, String>,
}

impl Dictionary {
    /// The dictionary sessions use: `file` with `layer` on top, resolved as the [module
    /// docs](self) describe. Fails only when `file` is invalid on its own.
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
            // database's. Of two database entries, the one saved later gives way.
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

/// The terms of `file` with `layer` on top, leaving out `skipped` entries, and when each term
/// taken from the layer was saved.
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
