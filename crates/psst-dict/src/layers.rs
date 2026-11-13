use std::collections::{BTreeMap, HashMap, HashSet};

use psst_config::{DictionaryFile, TermSpec};
use psst_proto::TermId;

use crate::{Dictionary, Error, NeverRule, Result, built_in_terms, term_id};

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
        let mut given_way = HashSet::new();
        loop {
            let Composed { specs, from_layer, built_in } = compose(file, layer, &skipped, &given_way);
            let err = match Dictionary::from_specs(specs) {
                Ok(dictionary) => {
                    let dictionary = dictionary.with_never_rules(layer.never.iter().cloned());
                    return Ok(Resolved { dictionary, skipped });
                }
                Err(err) => err,
            };
            let Error::SpellingConflict { first, second, .. } = &err else { return Err(err) };
            let ids = [term_id(first), term_id(second)];
            if let Some(id) = ids.iter().find(|id| built_in.contains(*id)) {
                given_way.insert(id.clone());
                continue;
            }
            // The file is valid alone and hiding only removes terms, so at least one side is the
            // database's, and it gives way: the file always wins. Saves are checked, so this
            // happens only when the file changed under the database.
            let loser =
                ids.into_iter().filter_map(|id| from_layer.get(&id).map(|&saved| (saved, id))).max().map(|(_, id)| id);
            match loser {
                Some(id) => skipped.insert(id, err.to_string()),
                None => return Err(err),
            };
        }
    }
}

struct Composed<'a> {
    specs: Vec<&'a TermSpec>,
    /// The identities whose version is the database's, with when it was saved.
    from_layer: HashMap<TermId, u64>,
    built_in: HashSet<TermId>,
}

/// Deterministic: the file's terms in its order, then the built-in terms the file lacks, then the
/// database-only terms by identity.
fn compose<'a>(
    file: &'a DictionaryFile,
    layer: &'a Layer,
    skipped: &BTreeMap<TermId, String>,
    given_way: &HashSet<TermId>,
) -> Composed<'a> {
    let entry = |id: &TermId| layer.terms.get(id).filter(|_| !skipped.contains_key(id));
    let built_ins = built_in_terms();
    let mut composed = Composed {
        specs: Vec::with_capacity(file.terms.len() + built_ins.len() + layer.terms.len()),
        from_layer: HashMap::new(),
        built_in: HashSet::new(),
    };
    let mut in_base = HashSet::with_capacity(file.terms.len() + built_ins.len());
    let file_terms = file.terms.iter().map(|spec| (term_id(&spec.text), spec, false));
    let built_in_terms = built_ins.iter().map(|spec| (term_id(&spec.text), spec, true));
    for (id, spec, built_in) in file_terms.chain(built_in_terms) {
        if !in_base.insert(id.clone()) {
            continue;
        }
        match entry(&id) {
            Some(LayerTerm { entry: LayerEntry::Hide, .. }) => {}
            Some(LayerTerm { entry: LayerEntry::Set(replacement), saved }) => {
                composed.specs.push(replacement);
                composed.from_layer.insert(id, *saved);
            }
            None if built_in && given_way.contains(&id) => {}
            None => {
                composed.specs.push(spec);
                if built_in {
                    composed.built_in.insert(id);
                }
            }
        }
    }
    for (id, term) in &layer.terms {
        if let LayerEntry::Set(spec) = &term.entry
            && !in_base.contains(id)
            && !skipped.contains_key(id)
        {
            composed.specs.push(spec);
            composed.from_layer.insert(id.clone(), term.saved);
        }
    }
    composed
}
