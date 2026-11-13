//! The dictionary page (plan 6.8): search; add, edit, hide or reset terms; never rules; export.
//! Every change goes through the same functions as `/api/dictionary`.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Form,
    extract::{
        Path, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::{
        StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS},
    },
    response::{IntoResponse, Redirect, Response},
};
use minijinja::{Value, context, value::Serde};
use psst_dict::term_id;
use psst_proto::api::{DictionaryTerm, NeverRuleId, Term, TermLayer, encode_segment};
use serde::{Deserialize, Serialize};

use super::{Viewer, chrome, render};
use crate::{
    api::ApiErr,
    dictionary::{self, Change},
    state::AppState,
};

type AppRef = Arc<AppState>;

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    q: String,
    edit: Option<String>,
}

/// A term's form: the fields as entered, plus the search to come back to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermForm {
    #[serde(default)]
    text: String,
    /// Spellings, comma-separated.
    #[serde(default)]
    sounds_like: String,
    #[serde(default)]
    languages: String,
    /// A checkbox: present when ticked.
    #[serde(default, with = "checkbox")]
    fuzzy: bool,
    #[serde(default, with = "checkbox")]
    pin: bool,
    #[serde(default)]
    notes: String,
    #[serde(default, skip_serializing)]
    q: String,
}

/// A checkbox is `on` when ticked and absent otherwise.
mod checkbox {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &bool, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(*value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
        Ok(!String::deserialize(deserializer)?.is_empty())
    }
}

impl TermForm {
    fn of(term: &Term) -> TermForm {
        TermForm {
            text: term.text.clone(),
            sounds_like: term.sounds_like.join(", "),
            languages: term.languages.clone(),
            fuzzy: term.fuzzy,
            pin: term.pin,
            notes: term.notes.clone().unwrap_or_default(),
            q: String::new(),
        }
    }

    fn term(&self) -> Term {
        let languages = self.languages.trim();
        Term {
            text: self.text.trim().to_owned(),
            sounds_like: self
                .sounds_like
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            languages: if languages.is_empty() { "any".into() } else { languages.to_owned() },
            fuzzy: self.fuzzy,
            pin: self.pin,
            notes: Some(self.notes.trim().to_owned()).filter(|notes| !notes.is_empty()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NeverForm {
    #[serde(default)]
    term: String,
    #[serde(default)]
    span: String,
    #[serde(default, skip_serializing)]
    q: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchForm {
    #[serde(default)]
    q: String,
}

/// One term as the page lists it.
#[derive(Debug, Serialize)]
struct Row {
    id: String,
    text: String,
    layer: TermLayer,
    active: bool,
    /// Whether it has a version to edit (not hidden).
    editable: bool,
    meta: String,
    problem: Option<String>,
    file_text: Option<String>,
    file_meta: String,
}

/// "sounds like hyper land · en only · pinned · no fuzzy matching · notes".
fn describe(term: &Term) -> String {
    let mut parts = Vec::new();
    if !term.sounds_like.is_empty() {
        parts.push(format!("sounds like {}", term.sounds_like.join(", ")));
    }
    if term.languages != "any" {
        parts.push(format!("{} only", term.languages));
    }
    if term.pin {
        parts.push("pinned".into());
    }
    if !term.fuzzy {
        parts.push("no fuzzy matching".into());
    }
    parts.extend(term.notes.clone());
    parts.join(" · ")
}

fn row(term: &DictionaryTerm) -> Row {
    Row {
        id: term.id.clone(),
        text: term.term.as_ref().map_or_else(|| term.id.clone(), |t| t.text.clone()),
        layer: term.layer,
        active: term.active,
        editable: term.term.is_some() && term.layer != TermLayer::Hidden,
        meta: term.term.as_ref().map(describe).unwrap_or_default(),
        problem: term.problem.clone(),
        file_text: term.file.as_ref().map(|file| file.text.clone()),
        file_meta: term.file.as_ref().map(describe).unwrap_or_default(),
    }
}

/// Whether a search matches a term: by identity, spelling or notes, ignoring case.
fn matches(term: &DictionaryTerm, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let versions = term.term.iter().chain(term.file.iter());
    term.id.contains(&q)
        || versions
            .flat_map(|t| std::iter::once(&t.text).chain(&t.sounds_like).chain(&t.notes))
            .any(|s| s.to_lowercase().contains(&q))
}

/// What a page shows besides the dictionary: a refusal and where, and the values entered.
#[derive(Debug, Default)]
struct Outcome {
    status: Option<StatusCode>,
    error: Option<String>,
    /// `add`, `never`, or a term's identity; none for the top of the page.
    error_for: Option<String>,
    add: Option<TermForm>,
    edit: Option<TermForm>,
    never: Option<NeverForm>,
}

fn page(state: &AppState, viewer: &Viewer, q: &str, edit: Option<&str>, outcome: Outcome) -> Response {
    let dictionary = state.live.dictionary();
    let view = state.live.layered().view(&dictionary);
    let edit = edit.map(term_id);
    let edit_fields = outcome.edit.or_else(|| {
        let term = view.terms.iter().find(|t| Some(&t.id) == edit.as_ref())?;
        term.term.as_ref().map(TermForm::of)
    });
    let texts = |id: &str| view.terms.iter().find(|t| t.id == id).and_then(|t| t.term.as_ref()).map(|t| t.text.clone());
    let never: Vec<Value> = view
        .never
        .iter()
        .map(|rule| {
            let text = texts(&rule.term).unwrap_or_else(|| rule.term.clone());
            context! { id => rule.id, text, span => rule.span.clone(), active => rule.active }
        })
        .collect();
    let mut languages: BTreeSet<String> = ["any", "en", "cs"].map(String::from).into();
    languages.extend(state.live.profile_layers().effective().profiles.values().map(|p| p.language.clone()));
    let ctx = context! {
        q,
        edit,
        total => view.terms.len(),
        terms => view.terms.iter().filter(|t| matches(t, q)).map(row).map(Serde).map(Value::from).collect::<Vec<_>>(),
        never,
        warnings => view.warnings.clone(),
        languages => languages.into_iter().collect::<Vec<_>>(),
        term_texts => dictionary.terms().iter().map(|t| t.text.clone()).collect::<Vec<_>>(),
        add_fields => Value::from(Serde(outcome.add.unwrap_or(TermForm { fuzzy: true, ..TermForm::default() }))),
        edit_fields => Value::from(Serde(edit_fields)),
        never_fields => Value::from(Serde(outcome.never.unwrap_or_default())),
        error => outcome.error,
        error_for => outcome.error_for,
        ..chrome(state, viewer, "dictionary")
    };
    render(outcome.status.unwrap_or(StatusCode::OK), "dictionary.html", ctx)
}

/// Back to the page, with the search kept.
fn back(q: &str) -> Response {
    let q = q.trim();
    let to = if q.is_empty() { "/dictionary".to_owned() } else { format!("/dictionary?q={}", encode_segment(q)) };
    Redirect::to(&to).into_response()
}

fn refused(err: ApiErr) -> (StatusCode, String) {
    let status =
        if err.status() == StatusCode::INTERNAL_SERVER_ERROR { err.status() } else { StatusCode::UNPROCESSABLE_ENTITY };
    (status, err.message().to_owned())
}

/// `GET /dictionary?q=&edit=`.
pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    page(&state, &viewer, &query.q, query.edit.as_deref(), Outcome::default())
}

/// A form that didn't parse: the page again, with the reason at the top.
fn bad_form(state: &AppState, viewer: &Viewer, err: FormRejection) -> Response {
    let outcome = Outcome { status: Some(StatusCode::BAD_REQUEST), error: Some(err.body_text()), ..Outcome::default() };
    page(state, viewer, "", None, outcome)
}

/// `POST /dictionary/terms`: adds a term.
pub async fn add(viewer: Viewer, State(state): State<AppRef>, form: Result<Form<TermForm>, FormRejection>) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    match dictionary::change_term(&state.live, &state.db, Change::Add(form.term().into())) {
        Ok(_) => back(&form.q),
        Err(err) => {
            let (status, error) = refused(err);
            let q = form.q.clone();
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some("add".into()),
                add: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &q, None, outcome)
        }
    }
}

/// `POST /dictionary/terms/{id}`: saves an edited term.
pub async fn save(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<TermForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    let id = term_id(&id);
    match dictionary::change_term(&state.live, &state.db, Change::Replace(id.clone(), form.term().into())) {
        Ok(_) => back(&form.q),
        Err(err) => {
            let (status, error) = refused(err);
            let q = form.q.clone();
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some(id.clone()),
                edit: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &q, Some(&id), outcome)
        }
    }
}

/// A one-button change to a term or a never rule; on refusal, the page with the reason on top.
fn button(state: &AppState, viewer: &Viewer, q: &str, result: Result<(), ApiErr>) -> Response {
    match result {
        Ok(()) => back(q),
        Err(err) => {
            let (status, error) = refused(err);
            page(state, viewer, q, None, Outcome { status: Some(status), error: Some(error), ..Outcome::default() })
        }
    }
}

/// `POST /dictionary/terms/{id}/hide`: hides a file term, or removes an added one.
pub async fn hide(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<SearchForm>, FormRejection>,
) -> Response {
    let q = form.map(|Form(form)| form.q).unwrap_or_default();
    let result = dictionary::change_term(&state.live, &state.db, Change::Remove(term_id(&id))).map(drop);
    button(&state, &viewer, &q, result)
}

/// `POST /dictionary/terms/{id}/reset`: brings back the file's version, if any.
pub async fn reset(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<SearchForm>, FormRejection>,
) -> Response {
    let q = form.map(|Form(form)| form.q).unwrap_or_default();
    let result = dictionary::change_term(&state.live, &state.db, Change::Reset(term_id(&id))).map(drop);
    button(&state, &viewer, &q, result)
}

/// `POST /dictionary/never`: adds a never rule.
pub async fn add_never(
    viewer: Viewer,
    State(state): State<AppRef>,
    form: Result<Form<NeverForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    match dictionary::add_never(&state.live, &state.db, form.term.trim(), form.span.trim()) {
        Ok(_) => back(&form.q),
        Err(err) => {
            let (status, error) = refused(err);
            let q = form.q.clone();
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some("never".into()),
                never: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &q, None, outcome)
        }
    }
}

/// `POST /dictionary/never/{id}/remove`.
pub async fn remove_never(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<NeverRuleId>,
    form: Result<Form<SearchForm>, FormRejection>,
) -> Response {
    let q = form.map(|Form(form)| form.q).unwrap_or_default();
    button(&state, &viewer, &q, dictionary::remove_never(&state.live, &state.db, id))
}

/// `GET /dictionary.toml`: the export, as a download.
pub async fn export(_: Viewer, State(state): State<AppRef>) -> Response {
    download("dictionary.toml", state.live.layered().export())
}

/// A TOML file to save.
pub(super) fn download(name: &str, toml: String) -> Response {
    (
        [
            (CONTENT_TYPE, "application/toml; charset=utf-8".to_owned()),
            (CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (CACHE_CONTROL, "no-store".to_owned()),
            (X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        toml,
    )
        .into_response()
}
