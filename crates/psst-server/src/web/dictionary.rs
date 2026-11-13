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
use psst_dict::{is_common_word, term_id};
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
    word: Option<String>,
    new: Option<String>,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermForm {
    #[serde(default)]
    text: String,
    #[serde(default)]
    sounds_like: String,
    #[serde(default)]
    languages: String,
    #[serde(default, with = "checkbox")]
    fuzzy: bool,
    #[serde(default, with = "checkbox")]
    pin: bool,
    #[serde(default)]
    notes: String,
    #[serde(default, skip_serializing)]
    q: String,
}

/// HTML forms send a ticked checkbox as `on` and leave an unticked one out.
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
pub struct ButtonForm {
    #[serde(default)]
    q: String,
    #[serde(default)]
    word: String,
}

#[derive(Debug, Serialize)]
struct Row {
    id: String,
    text: String,
    hidden: bool,
    problem: bool,
}

fn row(term: &DictionaryTerm) -> Row {
    Row {
        id: term.id.clone(),
        text: term.term.as_ref().map_or_else(|| term.id.clone(), |t| t.text.clone()),
        hidden: term.layer == TermLayer::Hidden,
        problem: term.problem.is_some(),
    }
}

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

fn toml_lines(term: &Term) -> String {
    let quote = |s: &str| format!("{s:?}");
    let mut lines = vec![format!("text = {}", quote(&term.text))];
    if !term.sounds_like.is_empty() {
        lines.push(format!(
            "sounds_like = [{}]",
            term.sounds_like.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ")
        ));
    }
    if term.languages != "any" {
        lines.push(format!("languages = {}", quote(&term.languages)));
    }
    if !term.fuzzy {
        lines.push("fuzzy = false".into());
    }
    if term.pin {
        lines.push("pin = true".into());
    }
    if let Some(notes) = &term.notes {
        lines.push(format!("notes = {}", quote(notes)));
    }
    lines.join("\n")
}

#[derive(Debug, Default)]
struct Outcome {
    status: Option<StatusCode>,
    error: Option<String>,
    /// A word's identity, `new` or `never`; `None` puts the error at the top of the page.
    error_for: Option<String>,
    form: Option<TermForm>,
    never: Option<NeverForm>,
}

fn page(state: &AppState, viewer: &Viewer, query: &PageQuery, outcome: Outcome) -> Response {
    let dictionary = state.live.dictionary();
    let view = state.live.layered().view(&dictionary);
    let q = query.q.as_str();
    let word = query.word.as_deref().map(term_id);
    let open = word.as_ref().and_then(|id| view.terms.iter().find(|t| &t.id == id));
    let new = query.new.is_some() || outcome.error_for.as_deref() == Some("new");

    let listed: Vec<&DictionaryTerm> = view.terms.iter().filter(|t| t.term.is_some() && matches(t, q)).collect();
    let mut shown: Vec<Row> = listed.iter().filter(|t| t.layer != TermLayer::Hidden).map(|t| row(t)).collect();
    shown.sort_by_key(|r| r.id.clone());
    let mut hidden: Vec<Row> = listed.iter().filter(|t| t.layer == TermLayer::Hidden).map(|t| row(t)).collect();
    hidden.sort_by_key(|r| r.id.clone());
    let attention: Vec<&DictionaryTerm> = view.terms.iter().filter(|t| t.problem.is_some()).collect();
    let exact = view.terms.iter().any(|t| t.term.as_ref().is_some_and(|term| term.text.eq_ignore_ascii_case(q.trim())));

    let mut languages: BTreeSet<String> = ["any", "en", "cs"].map(String::from).into();
    languages.extend(state.live.profile_layers().effective().profiles.values().map(|p| p.language.clone()));

    let panel = if new {
        let fields = outcome.form.clone().unwrap_or_else(|| TermForm {
            text: query.text.trim().to_owned(),
            languages: "any".into(),
            fuzzy: true,
            ..TermForm::default()
        });
        Some(context! {
            new => true,
            title => "Add a word",
            fields => Value::from(Serde(fields)),
            error => outcome.error_for.as_deref().filter(|f| *f == "new").and(outcome.error.clone()),
        })
    } else {
        open.map(|t| panel(state, &view, t, &outcome))
    };

    let has_panel = panel.is_some();
    // A hidden built-in term can't go into dictionary.toml, so it would ask for a download forever.
    let changes = view
        .terms
        .iter()
        .filter(|t| t.layer != TermLayer::File && !(t.built_in && t.layer == TermLayer::Hidden))
        .count();
    let ctx = context! {
        q,
        total => view.terms.iter().filter(|t| t.term.is_some() && t.layer != TermLayer::Hidden).count(),
        words => Value::from(Serde(shown)),
        hidden => Value::from(Serde(hidden)),
        attention => attention.first().map(|t| t.id.clone()),
        attention_count => attention.len(),
        offer_add => !q.trim().is_empty() && !exact,
        changes,
        warnings => view.warnings.clone(),
        languages => languages.into_iter().collect::<Vec<_>>(),
        panel,
        error => outcome.error.clone().filter(|_| outcome.error_for.is_none() || !has_panel),
        ..chrome(state, viewer, "dictionary")
    };
    render(outcome.status.unwrap_or(StatusCode::OK), "dictionary.html", ctx)
}

fn panel(state: &AppState, view: &psst_proto::api::DictionaryView, t: &DictionaryTerm, outcome: &Outcome) -> Value {
    let term = t.term.clone().unwrap_or_else(|| Term {
        text: t.id.clone(),
        sounds_like: Vec::new(),
        languages: "any".into(),
        fuzzy: true,
        pin: false,
        notes: None,
    });
    let mine = outcome.error_for.as_deref() == Some(t.id.as_str());
    let fields = outcome.form.clone().filter(|_| mine).unwrap_or_else(|| TermForm::of(&term));
    let uses = state
        .db
        .history(1000)
        .unwrap_or_default()
        .iter()
        .filter(|row| {
            row.r#final
                .as_ref()
                .is_some_and(|fin| fin.edits.iter().any(|edit| edit.term.as_deref() == Some(t.id.as_str())))
        })
        .count();
    let sub = match (t.layer, uses) {
        (TermLayer::Hidden, _) => "Hidden: psst doesn't use it".to_owned(),
        (_, 0) => "Hasn't changed a dictation in the last 30 days".to_owned(),
        (_, 1) => "Fixed 1 dictation in the last 30 days".to_owned(),
        (_, n) => format!("Fixed {n} dictations in the last 30 days"),
    };
    let never: Vec<Value> = view
        .never
        .iter()
        .filter(|rule| rule.term == t.id)
        .map(|rule| context! { id => rule.id, span => rule.span.clone(), active => rule.active })
        .collect();
    let source = match (t.layer, t.built_in) {
        (TermLayer::File, false) => "From your Nix config (dictionary.toml), unchanged.",
        (TermLayer::File, true) => "Built into psst.",
        (TermLayer::Added, _) => "Added here, so it lives only on this server until you move it to your Nix config.",
        (TermLayer::Replaced, false) => "Changed here. Your Nix config has this version:",
        (TermLayer::Replaced, true) => "Changed here. psst has this version built in:",
        (TermLayer::Hidden, false) => "Hidden here. Your Nix config still has it.",
        (TermLayer::Hidden, true) => "Hidden here. psst has it built in.",
    };
    let common: Vec<String> = term
        .sounds_like
        .iter()
        .chain(std::iter::once(&term.text))
        .filter(|s| ["en", "cs"].iter().any(|lang| is_common_word(lang, s)))
        .cloned()
        .collect();
    context! {
        id => t.id.clone(),
        title => term.text.clone(),
        sub,
        layer => Value::from(Serde(t.layer)),
        built_in => t.built_in,
        fields => Value::from(Serde(fields)),
        error => mine.then(|| outcome.error.clone()).flatten(),
        never,
        never_error => (outcome.error_for.as_deref() == Some("never")).then(|| outcome.error.clone()).flatten(),
        never_span => outcome.never.as_ref().map(|n| n.span.clone()).unwrap_or_default(),
        problem => t.problem.clone(),
        source,
        file => t.file.as_ref().map(toml_lines),
        common,
    }
}

fn back(q: &str, word: &str) -> Response {
    let mut params = Vec::new();
    if !q.trim().is_empty() {
        params.push(format!("q={}", encode_segment(q.trim())));
    }
    if !word.is_empty() {
        params.push(format!("word={}", encode_segment(word)));
    }
    let to = if params.is_empty() { "/dictionary".to_owned() } else { format!("/dictionary?{}", params.join("&")) };
    Redirect::to(&to).into_response()
}

fn refused(err: ApiErr) -> (StatusCode, String) {
    let status =
        if err.status() == StatusCode::INTERNAL_SERVER_ERROR { err.status() } else { StatusCode::UNPROCESSABLE_ENTITY };
    (status, err.message().to_owned())
}

pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    page(&state, &viewer, &query, Outcome::default())
}

fn bad_form(state: &AppState, viewer: &Viewer, err: FormRejection) -> Response {
    let outcome = Outcome { status: Some(StatusCode::BAD_REQUEST), error: Some(err.body_text()), ..Outcome::default() };
    page(state, viewer, &PageQuery::default(), outcome)
}

pub async fn add(viewer: Viewer, State(state): State<AppRef>, form: Result<Form<TermForm>, FormRejection>) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    match dictionary::change_term(&state.live, &state.db, Change::Add(form.term().into())) {
        Ok(_) => back(&form.q, ""),
        Err(err) => {
            let (status, error) = refused(err);
            let query = PageQuery { q: form.q.clone(), ..PageQuery::default() };
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some("new".into()),
                form: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &query, outcome)
        }
    }
}

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
    let term = form.term();
    let result = if !term.text.is_empty() && term_id(&term.text) != id {
        rename(&state, &id, term)
    } else {
        dictionary::change_term(&state.live, &state.db, Change::Replace(id.clone(), term.into())).map(drop)
    };
    match result {
        Ok(_) => back(&form.q, ""),
        Err(err) => {
            let (status, error) = refused(err);
            let query = PageQuery { q: form.q.clone(), word: Some(id.clone()), ..PageQuery::default() };
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some(id.clone()),
                form: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &query, outcome)
        }
    }
}

/// One change, so the old word's heard spellings are free for the new one.
fn rename(state: &AppState, id: &str, term: Term) -> Result<(), ApiErr> {
    let changes = vec![Change::Remove(id.to_owned()), Change::Add(term.into())];
    dictionary::change_terms(&state.live, &state.db, changes).map(drop)
}

fn button(state: &AppState, viewer: &Viewer, form: &ButtonForm, result: Result<(), ApiErr>) -> Response {
    match result {
        Ok(()) => back(&form.q, &form.word),
        Err(err) => {
            let (status, error) = refused(err);
            let query = PageQuery { q: form.q.clone(), ..PageQuery::default() };
            page(state, viewer, &query, Outcome { status: Some(status), error: Some(error), ..Outcome::default() })
        }
    }
}

pub async fn hide(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<ButtonForm>, FormRejection>,
) -> Response {
    let form = form.map(|Form(form)| form).unwrap_or_default();
    let result = dictionary::change_term(&state.live, &state.db, Change::Remove(term_id(&id))).map(drop);
    button(&state, &viewer, &ButtonForm { word: String::new(), ..form }, result)
}

pub async fn reset(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<ButtonForm>, FormRejection>,
) -> Response {
    let form = form.map(|Form(form)| form).unwrap_or_default();
    let result = dictionary::change_term(&state.live, &state.db, Change::Reset(term_id(&id))).map(drop);
    button(&state, &viewer, &form, result)
}

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
        Ok(_) => back(&form.q, &term_id(form.term.trim())),
        Err(err) => {
            let (status, error) = refused(err);
            let query = PageQuery { q: form.q.clone(), word: Some(form.term.clone()), ..PageQuery::default() };
            let outcome = Outcome {
                status: Some(status),
                error: Some(error),
                error_for: Some("never".into()),
                never: Some(form),
                ..Outcome::default()
            };
            page(&state, &viewer, &query, outcome)
        }
    }
}

pub async fn remove_never(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<NeverRuleId>,
    form: Result<Form<ButtonForm>, FormRejection>,
) -> Response {
    let form = form.map(|Form(form)| form).unwrap_or_default();
    button(&state, &viewer, &form, dictionary::remove_never(&state.live, &state.db, id))
}

pub async fn export(_: Viewer, State(state): State<AppRef>) -> Response {
    download("dictionary.toml", state.live.layered().export())
}

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
