//! The dictionary's database layer and the `/api/dictionary` operations.
//!
//! [`Layered`] holds `dictionary.toml` (its last valid version), the database's entries and never
//! rules, and which entries resolution leaves out. It lives in `LiveConfig` behind one mutex, which
//! every change and every `SIGHUP` reload holds from reading it to swapping the new [`Dictionary`]
//! in. A change is checked on the dictionary it would give, written to the database, and only then
//! swapped in; sessions keep the `Arc<Dictionary>` they took at `start`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, anyhow};
use psst_config::{ConfigFile, DictionaryFile, TermSpec};
use psst_dict::{Dictionary, Layer, LayerEntry, LayerTerm, NeverRule, Resolved, fuzzy_key, term_id};
use psst_proto::{
    TermId,
    api::{
        ApiErrorCode, DictionaryTerm, DictionaryView, NeverRule as ApiNeverRule, NeverRuleId, Term, TermChange,
        TermLayer,
    },
};
use serde::Serialize;

use crate::{
    api::ApiErr,
    db::{Db, StoredNever},
    state::LiveConfig,
};

/// `dictionary.toml` with the database layer on top.
#[derive(Debug)]
pub struct Layered {
    /// The last valid `dictionary.toml`.
    file: DictionaryFile,
    terms: BTreeMap<TermId, LayerTerm>,
    never: Vec<StoredNever>,
    /// Entries the last resolution left out, with the reason.
    skipped: BTreeMap<TermId, String>,
}

impl Layered {
    /// Reads the database layer and resolves it on top of `file`, which must be valid.
    pub fn load(file: DictionaryFile, db: &Db) -> anyhow::Result<(Layered, Dictionary)> {
        let mut layered =
            Layered { file, terms: db.dictionary_terms()?, never: db.never_rules()?, skipped: BTreeMap::new() };
        let resolved = layered.resolve().map_err(|err| anyhow!("{}: {err}", DictionaryFile::FILE_NAME))?;
        let dictionary = layered.install(resolved);
        Ok((layered, dictionary))
    }

    fn layer(&self) -> Layer {
        Layer {
            terms: self.terms.clone(),
            never: self
                .never
                .iter()
                .map(|n| NeverRule { term: n.term.clone(), span_key: n.span_key.clone() })
                .collect(),
        }
    }

    fn resolve(&self) -> psst_dict::Result<Resolved> {
        Dictionary::resolve(&self.file, &self.layer())
    }

    /// Keeps what `resolved` left out, logs the entries newly left out, and returns its dictionary.
    fn install(&mut self, resolved: Resolved) -> Dictionary {
        for (id, reason) in &resolved.skipped {
            if !self.skipped.contains_key(id) {
                tracing::error!("dictionary: the database's entry for `{id}` is left out: {reason}");
            }
        }
        self.skipped = resolved.skipped;
        resolved.dictionary
    }

    /// Re-reads `dictionary.toml` after `SIGHUP`. A file that fails validation, on its own,
    /// keeps the previous version.
    pub fn reload(&mut self, file: Result<DictionaryFile, String>) -> Option<Dictionary> {
        let file = match file {
            Ok(file) => file,
            Err(err) => {
                tracing::error!("{err}; keeping the previous {}", DictionaryFile::FILE_NAME);
                return None;
            }
        };
        let previous = std::mem::replace(&mut self.file, file);
        match self.resolve() {
            Ok(resolved) => {
                tracing::info!("{}: reloaded", DictionaryFile::FILE_NAME);
                Some(self.install(resolved))
            }
            Err(err) => {
                tracing::error!(
                    "{}: {err}; keeping the previous {}",
                    DictionaryFile::FILE_NAME,
                    DictionaryFile::FILE_NAME
                );
                self.file = previous;
                None
            }
        }
    }

    fn file_term(&self, id: &str) -> Option<&TermSpec> {
        self.file.terms.iter().find(|spec| term_id(&spec.text) == id)
    }

    /// One identity's entry as the API shows it, if either layer knows it.
    fn entry(&self, id: &str, file_term: Option<&TermSpec>) -> Option<DictionaryTerm> {
        let problem = self.skipped.get(id).cloned();
        let (layer, term, file) = match (file_term, self.terms.get(id).map(|t| &t.entry)) {
            (None, None) => return None,
            (Some(file), None) => (TermLayer::File, Some(file), None),
            (file, Some(LayerEntry::Hide)) => (TermLayer::Hidden, file, None),
            (None, Some(LayerEntry::Set(spec))) => (TermLayer::Added, Some(spec), None),
            (Some(file), Some(LayerEntry::Set(spec))) => (TermLayer::Replaced, Some(spec), Some(Term::from(file))),
        };
        let active = match layer {
            TermLayer::File => true,
            TermLayer::Hidden => false,
            TermLayer::Added | TermLayer::Replaced => problem.is_none(),
        };
        Some(DictionaryTerm { id: id.to_owned(), layer, active, term: term.map(Term::from), file, problem })
    }

    fn never_rule(&self, rule: &StoredNever, dictionary: &Dictionary) -> ApiNeverRule {
        ApiNeverRule {
            id: rule.id,
            term: rule.term.clone(),
            span: rule.span.clone(),
            span_key: rule.span_key.clone(),
            active: dictionary.terms().iter().any(|t| t.id == rule.term),
            created_at: rule.created_at,
        }
    }

    /// `GET /api/dictionary`.
    pub fn view(&self, dictionary: &Dictionary) -> DictionaryView {
        let file_terms: HashMap<TermId, &TermSpec> =
            self.file.terms.iter().map(|spec| (term_id(&spec.text), spec)).collect();
        let ids: BTreeSet<&TermId> = file_terms.keys().chain(self.terms.keys()).collect();
        DictionaryView {
            terms: ids.into_iter().filter_map(|id| self.entry(id, file_terms.get(id).copied())).collect(),
            never: self.never.iter().map(|rule| self.never_rule(rule, dictionary)).collect(),
            warnings: dictionary.warnings().into_iter().map(|w| w.message).collect(),
        }
    }

    /// `GET /api/dictionary/export`: the database layer as `dictionary.toml`.
    pub fn export(&self) -> String {
        let mut out = format!(
            "# The psst dictionary's database layer, exported by psst-server {}.\n\
             # Move these terms into dictionary.toml (the NixOS module's settings.dictionary), in place\n\
             # of the file's versions of replaced terms; then `psst dict reset <term>` drops each entry.\n",
            env!("CARGO_PKG_VERSION")
        );
        let mut hidden = Vec::new();
        for (id, term) in &self.terms {
            let spec = match &term.entry {
                LayerEntry::Set(spec) => spec,
                LayerEntry::Hide => {
                    hidden.extend(self.file_term(id).map(|file| file.text.clone()));
                    continue;
                }
            };
            out.push('\n');
            if let Some(file) = self.file_term(id) {
                out.push_str(&format!("# Replaces dictionary.toml's {:?}.\n", file.text));
            }
            if let Some(reason) = self.skipped.get(id) {
                out.push_str(&format!("# Left out of the dictionary: {}\n", comment(reason)));
            }
            out.push_str(&toml_term(spec));
        }
        if !hidden.is_empty() {
            out.push_str("\n# Hidden: remove these from dictionary.toml.\n");
            for text in hidden {
                out.push_str(&format!("# - {text:?}\n"));
            }
        }
        if !self.never.is_empty() {
            out.push_str("\n# Never rules, which dictionary.toml can't express; they stay in the database.\n");
            for rule in &self.never {
                out.push_str(&format!("# - {:?} never replaces {:?}\n", rule.term, rule.span));
            }
        }
        out
    }
}

/// `text` on one comment line.
fn comment(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// One `[[terms]]` table, with the keys at their default left out.
fn toml_term(spec: &TermSpec) -> String {
    #[derive(Serialize)]
    struct Terms {
        terms: [Term; 1],
    }
    toml::to_string(&Terms { terms: [Term::from(spec)] }).expect("terms serialise as TOML")
}

/// A change to one term identity.
#[derive(Debug, Clone)]
pub enum Change {
    /// `POST /api/dictionary/terms`.
    Add(TermSpec),
    /// `PUT /api/dictionary/terms/{id}`.
    Replace(TermId, TermSpec),
    /// `DELETE /api/dictionary/terms/{id}`.
    Remove(TermId),
    /// `POST /api/dictionary/terms/{id}/reset`.
    Reset(TermId),
}

fn bad_request(message: impl Into<String>) -> ApiErr {
    ApiErr::new(ApiErrorCode::BadRequest, message)
}

fn not_found(message: impl Into<String>) -> ApiErr {
    ApiErr::new(ApiErrorCode::NotFound, message)
}

/// The checks `dictionary.toml` gets on its own: a non-empty `text` and `sounds_like` entries,
/// and a language.
fn check_term(spec: &TermSpec) -> Result<(), ApiErr> {
    DictionaryFile { terms: vec![spec.clone()] }.check().map_err(bad_request)
}

/// Applies `change`: checks it on the dictionary it would give, stores it and swaps that
/// dictionary in.
pub fn change_term(live: &LiveConfig, db: &Db, change: Change) -> Result<TermChange, ApiErr> {
    let mut layered = live.layered();
    let (id, entry) = match change {
        Change::Add(spec) => {
            check_term(&spec)?;
            let id = term_id(&spec.text);
            let known = match layered.terms.get(&id).map(|t| &t.entry) {
                Some(LayerEntry::Set(_)) => true,
                Some(LayerEntry::Hide) => false,
                None => layered.file_term(&id).is_some(),
            };
            if known {
                return Err(bad_request(format!(
                    "`{}` is already in the dictionary; replace it with PUT {}",
                    spec.text,
                    psst_proto::api::routes::with_id(psst_proto::api::routes::DICTIONARY_TERM, &id)
                )));
            }
            (id, Some(LayerEntry::Set(spec)))
        }
        Change::Replace(id, spec) => {
            check_term(&spec)?;
            let new_id = term_id(&spec.text);
            if new_id != id {
                return Err(bad_request(format!(
                    "`{}` has the identity `{new_id}`, not `{id}`; renaming is a hide plus an add",
                    spec.text
                )));
            }
            if !layered.terms.contains_key(&id) && layered.file_term(&id).is_none() {
                return Err(not_found(format!("no term `{id}`; add it with POST /api/dictionary/terms")));
            }
            (id, Some(LayerEntry::Set(spec)))
        }
        Change::Remove(id) => {
            let entry = match (layered.file_term(&id).is_some(), layered.terms.get(&id).map(|t| &t.entry)) {
                (true, Some(LayerEntry::Hide)) => {
                    return Err(not_found(format!("`{id}` is already hidden; reset it to bring it back")));
                }
                (true, _) => Some(LayerEntry::Hide),
                (false, Some(LayerEntry::Set(_))) => None,
                (false, _) => return Err(not_found(format!("no term `{id}`"))),
            };
            (id, entry)
        }
        Change::Reset(id) => {
            if !layered.terms.contains_key(&id) {
                return Err(not_found(format!("`{id}` has no database entry to reset")));
            }
            (id, None)
        }
    };

    let saved = layered.terms.values().map(|t| t.saved).max().unwrap_or(0) + 1;
    let mut proposed = layered.layer();
    match &entry {
        Some(entry) => proposed.terms.insert(id.clone(), LayerTerm { entry: entry.clone(), saved }),
        None => proposed.terms.remove(&id),
    };
    let resolved = Dictionary::resolve(&layered.file, &proposed).context("the dictionary file stopped resolving")?;
    let saves_a_term = matches!(entry, Some(LayerEntry::Set(_)));
    if let Some((_, reason)) = resolved
        .skipped
        .iter()
        .find(|(skipped, _)| (saves_a_term && **skipped == id) || !layered.skipped.contains_key(*skipped))
    {
        return Err(bad_request(reason.clone()));
    }

    match &entry {
        Some(entry) => db.put_dictionary_term(&id, entry, saved)?,
        None => {
            db.delete_dictionary_term(&id)?;
        }
    }
    let before: BTreeSet<String> = live.dictionary().warnings().into_iter().map(|w| w.message).collect();
    let warnings =
        resolved.dictionary.warnings().into_iter().map(|w| w.message).filter(|w| !before.contains(w)).collect();
    match entry {
        Some(entry) => layered.terms.insert(id.clone(), LayerTerm { entry, saved }),
        None => layered.terms.remove(&id),
    };
    let dictionary = layered.install(resolved);
    live.set_dictionary(dictionary);
    let term = layered.entry(&id, layered.file_term(&id));
    match &term {
        Some(term) => tracing::info!("dictionary: `{id}` is now {:?}", term.layer),
        None => tracing::info!("dictionary: `{id}` removed"),
    }
    Ok(TermChange { term, warnings })
}

/// `POST /api/dictionary/never`.
pub fn add_never(live: &LiveConfig, db: &Db, term: &str, span: &str) -> Result<ApiNeverRule, ApiErr> {
    let mut layered = live.layered();
    let id = term_id(term);
    if !live.dictionary().terms().iter().any(|t| t.id == id) {
        return Err(not_found(format!("no term `{term}` in the dictionary")));
    }
    let span_key = fuzzy_key(span);
    if span_key.is_empty() {
        return Err(bad_request("the span needs at least one letter or digit"));
    }
    let (rule, added) = db.add_never_rule(&id, span, &span_key)?;
    if added {
        layered.never.push(rule.clone());
        swap(&mut layered, live)?;
        tracing::info!("dictionary: `{id}` never replaces {span:?}");
    }
    Ok(layered.never_rule(&rule, &live.dictionary()))
}

/// `DELETE /api/dictionary/never/{id}`.
pub fn remove_never(live: &LiveConfig, db: &Db, id: NeverRuleId) -> Result<(), ApiErr> {
    let mut layered = live.layered();
    if !db.delete_never_rule(id)? {
        return Err(not_found(format!("no never rule {id}")));
    }
    layered.never.retain(|rule| rule.id != id);
    swap(&mut layered, live)?;
    tracing::info!("dictionary: never rule {id} removed");
    Ok(())
}

/// Resolves the layer again and swaps the result in.
fn swap(layered: &mut Layered, live: &LiveConfig) -> Result<(), ApiErr> {
    let resolved = layered.resolve().context("the dictionary file stopped resolving")?;
    live.set_dictionary(layered.install(resolved));
    Ok(())
}
