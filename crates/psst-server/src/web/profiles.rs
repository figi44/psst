//! The profiles page (plan 6.8): every profile, each editable, reset for the database's versions,
//! a form to add one, and the export. Every change goes through the same function as
//! `/api/profiles`.

use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Form,
    extract::{Path, State, rejection::FormRejection},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use minijinja::{Value, context, value::Serde};
use psst_config::{PROMPT_STYLE_WARN_TOKENS, Rule, estimated_tokens};
use psst_proto::api::{Profile, ProfileLayer};
use serde::{Deserialize, Serialize};

use super::{Viewer, chrome, dictionary::download, render};
use crate::{api::ApiErr, profiles, state::AppState};

type AppRef = Arc<AppState>;

/// A profile's form, as entered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileForm {
    /// Only in the form that adds a profile.
    #[serde(default, skip_serializing)]
    name: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    prompt_style: String,
    /// Rule ids, separated by commas or spaces.
    #[serde(default)]
    rules: String,
    /// The style's estimated length in tokens, for the page.
    #[serde(default, skip_deserializing)]
    tokens: usize,
}

impl ProfileForm {
    fn of(profile: &Profile) -> ProfileForm {
        ProfileForm {
            name: String::new(),
            language: profile.language.clone(),
            prompt_style: profile.prompt_style.clone(),
            rules: profile.rules.join(", "),
            tokens: 0,
        }
        .measured()
    }

    fn measured(mut self) -> ProfileForm {
        self.tokens = estimated_tokens(&self.prompt_style);
        self
    }

    /// The profile as entered: the style exactly as written, since it's the start of the prompt.
    fn profile(&self) -> Profile {
        Profile {
            language: self.language.trim().to_owned(),
            prompt_style: self.prompt_style.clone(),
            rules: self
                .rules
                .split([',', ' ', '\t', '\n'])
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(String::from)
                .collect(),
        }
    }
}

#[derive(Debug, Default)]
struct Outcome {
    status: Option<StatusCode>,
    error: Option<String>,
    /// A profile's name, or `new`; none for the top of the page.
    error_for: Option<String>,
    /// The values entered in the form the error is about.
    form: Option<ProfileForm>,
}

fn page(state: &AppState, viewer: &Viewer, outcome: Outcome) -> Response {
    let view = state.live.profile_layers().view();
    let mut form = outcome.form;
    let entered = |name: &str, form: &mut Option<ProfileForm>| {
        (outcome.error_for.as_deref() == Some(name)).then(|| form.take()).flatten().map(ProfileForm::measured)
    };
    let cards: Vec<Value> = view
        .profiles
        .iter()
        .map(|entry| {
            let fields = entered(&entry.name, &mut form).unwrap_or_else(|| ProfileForm::of(&entry.profile));
            let file = entry.file.as_ref().map(|file| {
                context! {
                    language => file.language.clone(),
                    rules => file.rules.join(", "),
                    prompt_style => format!("{:?}", file.prompt_style),
                }
            });
            context! {
                name => entry.name.clone(),
                layer => Value::from(Serde(entry.layer)),
                same_as_file => entry.layer == ProfileLayer::Replaced && entry.file.as_ref() == Some(&entry.profile),
                fields => Value::from(Serde(fields)),
                file,
            }
        })
        .collect();
    let new_form = entered("new", &mut form);
    let new_name = new_form.as_ref().map(|form| form.name.clone()).unwrap_or_default();
    let new_fields = new_form.unwrap_or_else(|| ProfileForm::of(&Profile::default()));
    let mut languages: BTreeSet<String> = ["en", "cs"].map(String::from).into();
    languages.extend(view.profiles.iter().map(|p| p.profile.language.clone()));
    let ctx = context! {
        profiles => cards,
        warnings => view.warnings,
        rule_ids => Rule::ALL.map(Rule::id).to_vec(),
        warn_tokens => PROMPT_STYLE_WARN_TOKENS,
        languages => languages.into_iter().collect::<Vec<_>>(),
        new_name,
        new_fields => Value::from(Serde(new_fields)),
        error => outcome.error,
        error_for => outcome.error_for,
        ..chrome(state, viewer, "profiles")
    };
    render(outcome.status.unwrap_or(StatusCode::OK), "profiles.html", ctx)
}

fn back() -> Response {
    Redirect::to("/profiles").into_response()
}

fn refused(
    state: &AppState,
    viewer: &Viewer,
    err: ApiErr,
    error_for: Option<&str>,
    form: Option<ProfileForm>,
) -> Response {
    let status =
        if err.status() == StatusCode::INTERNAL_SERVER_ERROR { err.status() } else { StatusCode::UNPROCESSABLE_ENTITY };
    let outcome = Outcome {
        status: Some(status),
        error: Some(err.message().to_owned()),
        error_for: error_for.map(String::from),
        form,
    };
    page(state, viewer, outcome)
}

fn bad_form(state: &AppState, viewer: &Viewer, err: FormRejection) -> Response {
    page(
        state,
        viewer,
        Outcome { status: Some(StatusCode::BAD_REQUEST), error: Some(err.body_text()), ..Outcome::default() },
    )
}

/// `GET /profiles`.
pub async fn show(viewer: Viewer, State(state): State<AppRef>) -> Response {
    page(&state, &viewer, Outcome::default())
}

/// `POST /profiles/{name}`: saves a profile over profiles.toml's.
pub async fn save(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(name): Path<String>,
    form: Result<Form<ProfileForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    match profiles::change(&state.live, &state.db, &name, Some(form.profile())) {
        Ok(_) => back(),
        Err(err) => refused(&state, &viewer, err, Some(&name), Some(form)),
    }
}

/// `POST /profiles`: adds a profile under a name that isn't in use.
pub async fn add(
    viewer: Viewer,
    State(state): State<AppRef>,
    form: Result<Form<ProfileForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(err) => return bad_form(&state, &viewer, err),
    };
    let name = form.name.trim().to_owned();
    let error = if name.is_empty() {
        Some("the profile needs a name".to_owned())
    } else if state.live.profile_layers().effective().profiles.contains_key(&name) {
        Some(format!("there's a profile `{name}` already; change it above"))
    } else {
        None
    };
    if let Some(error) = error {
        let err = ApiErr::new(psst_proto::api::ApiErrorCode::BadRequest, error);
        return refused(&state, &viewer, err, Some("new"), Some(form));
    }
    match profiles::change(&state.live, &state.db, &name, Some(form.profile())) {
        Ok(_) => back(),
        Err(err) => refused(&state, &viewer, err, Some("new"), Some(form)),
    }
}

/// `POST /profiles/{name}/reset`: back to profiles.toml's version, or gone if it has none.
pub async fn reset(viewer: Viewer, State(state): State<AppRef>, Path(name): Path<String>) -> Response {
    match profiles::change(&state.live, &state.db, &name, None) {
        Ok(_) => back(),
        Err(err) => refused(&state, &viewer, err, None, None),
    }
}

/// `GET /profiles.toml`: the export, as a download.
pub async fn export(_: Viewer, State(state): State<AppRef>) -> Response {
    download("profiles.toml", state.live.profile_layers().export())
}
