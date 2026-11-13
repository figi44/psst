//! The profiles page (plan 6.8): every profile as a list, and a side panel for one (`?profile=`)
//! or a new one (`?new`): language, prompt style and clean-up rules, with the rules' order and
//! profiles.toml's version under Advanced; reset for the database's versions, and the export.
//! Every change goes through the same function as `/api/profiles`.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use axum::{
    Form,
    extract::{
        Path, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
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

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// The profile whose panel is open.
    profile: Option<String>,
    /// Any value opens the panel for a new profile.
    new: Option<String>,
}

/// What each clean-up rule is called on the page, and what it does.
fn describe(rule: &str) -> (&'static str, &'static str) {
    match rule {
        "fillers" => ("Remove fillers", "Removes filler words like um and uh."),
        "whitespace" => ("Tidy spaces", "Removes extra spaces."),
        "capitalize" => {
            ("Capitalise the start", "Starts the text with a capital letter, unless it starts with a dictionary word.")
        }
        "trailing_space" => {
            ("Trailing space", "Adds a space at the end, so the next dictation doesn't run into this one.")
        }
        _ => ("", ""),
    }
}

/// A profile's form, as entered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileForm {
    /// Only in the form that adds a profile.
    name: String,
    language: String,
    prompt_style: String,
    /// Every rule id, in the order they run: the profile's rules first, then the others.
    order: Vec<String>,
    /// The rules switched on.
    on: BTreeSet<String>,
}

impl ProfileForm {
    fn of(profile: &Profile) -> ProfileForm {
        let mut order = profile.rules.clone();
        order.extend(
            Rule::ALL
                .map(Rule::id)
                .iter()
                .filter(|id| !profile.rules.iter().any(|r| r == *id))
                .map(|id| id.to_string()),
        );
        ProfileForm {
            name: String::new(),
            language: profile.language.clone(),
            prompt_style: profile.prompt_style.clone(),
            order,
            on: profile.rules.iter().cloned().collect(),
        }
    }

    /// The fields as posted: `name`, `language`, `prompt_style`, `order` (rule ids separated by
    /// commas or spaces) and a `rule_<id>` checkbox for each rule switched on.
    fn from_fields(fields: &HashMap<String, String>) -> ProfileForm {
        let get = |key: &str| fields.get(key).cloned().unwrap_or_default();
        let mut order: Vec<String> = get("order")
            .split([',', ' ', '\t', '\n'])
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(String::from)
            .collect();
        for id in Rule::ALL.map(Rule::id) {
            if !order.iter().any(|r| r == id) {
                order.push(id.to_owned());
            }
        }
        let on = fields
            .iter()
            .filter(|(key, value)| key.starts_with("rule_") && !value.is_empty())
            .map(|(key, _)| key["rule_".len()..].to_owned())
            .collect();
        ProfileForm { name: get("name"), language: get("language"), prompt_style: get("prompt_style"), order, on }
    }

    /// The profile as entered: the style exactly as written, since it's the start of the prompt.
    fn profile(&self) -> Profile {
        Profile {
            language: self.language.trim().to_owned(),
            prompt_style: self.prompt_style.clone(),
            rules: self.order.iter().filter(|id| self.on.contains(*id)).cloned().collect(),
        }
    }

    /// What the template needs: the fields, each rule with its name and switch, and the size.
    fn view(&self) -> Value {
        let rules: Vec<Value> = self
            .order
            .iter()
            .map(|id| {
                let (name, what) = describe(id);
                context! { id, name, what, on => self.on.contains(id) }
            })
            .collect();
        let tokens = estimated_tokens(&self.prompt_style);
        context! {
            name => self.name.clone(),
            language => self.language.clone(),
            prompt_style => self.prompt_style.clone(),
            order => self.order.join(", "),
            rules,
            tokens,
            over => tokens > PROMPT_STYLE_WARN_TOKENS,
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

fn page(state: &AppState, viewer: &Viewer, query: &PageQuery, outcome: Outcome) -> Response {
    let view = state.live.profile_layers().view();
    let list: Vec<Value> = view
        .profiles
        .iter()
        .map(|entry| {
            context! {
                name => entry.name.clone(),
                language => language_name(&entry.profile.language),
                style => entry.profile.prompt_style.clone(),
            }
        })
        .collect();
    let open_name = outcome.error_for.clone().filter(|n| n != "new").or(query.profile.clone());
    let new = query.new.is_some() || outcome.error_for.as_deref() == Some("new");
    let panel = if new {
        let fields = outcome.form.clone().unwrap_or_else(|| ProfileForm::of(&Profile::default()));
        Some(context! {
            new => true,
            title => "New profile",
            fields => fields.view(),
            error => outcome.error.clone().filter(|_| outcome.error_for.as_deref() == Some("new")),
        })
    } else {
        open_name.and_then(|name| view.profiles.iter().find(|p| p.name == name)).map(|entry| {
            let mine = outcome.error_for.as_deref() == Some(entry.name.as_str());
            let fields = outcome.form.clone().filter(|_| mine).unwrap_or_else(|| ProfileForm::of(&entry.profile));
            let file = entry.file.as_ref().map(|file| {
                format!(
                    "language = {:?}\nprompt_style = {:?}\nrules = [{}]",
                    file.language,
                    file.prompt_style,
                    file.rules.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>().join(", ")
                )
            });
            let source = match entry.layer {
                ProfileLayer::File => "From your Nix config (profiles.toml), unchanged.",
                ProfileLayer::Added => "Added here, so it lives only on this server until you move it to your Nix config.",
                ProfileLayer::Replaced => "Changed here. Your Nix config has this version:",
            };
            context! {
                name => entry.name.clone(),
                title => entry.name.clone(),
                sub => if entry.name == "default" { "Used when you run psst toggle".to_owned() } else { format!("Used with psst toggle --profile {}", entry.name) },
                layer => Value::from(Serde(entry.layer)),
                same_as_file => entry.layer == ProfileLayer::Replaced && entry.file.as_ref() == Some(&entry.profile),
                fields => fields.view(),
                error => outcome.error.clone().filter(|_| mine),
                source,
                file,
            }
        })
    };
    let mut languages: BTreeSet<String> = ["en", "cs"].map(String::from).into();
    languages.extend(view.profiles.iter().map(|p| p.profile.language.clone()));
    let ctx = context! {
        profiles => list,
        changes => view.profiles.iter().filter(|p| p.layer != ProfileLayer::File).count(),
        warnings => view.warnings,
        warn_tokens => PROMPT_STYLE_WARN_TOKENS,
        languages => languages.into_iter().collect::<Vec<_>>(),
        panel,
        error => outcome.error.clone().filter(|_| outcome.error_for.is_none()),
        ..chrome(state, viewer, "profiles")
    };
    render(outcome.status.unwrap_or(StatusCode::OK), "profiles.html", ctx)
}

fn language_name(code: &str) -> String {
    match code {
        "en" => "English".into(),
        "cs" => "Czech".into(),
        other => other.into(),
    }
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
    page(state, viewer, &PageQuery::default(), outcome)
}

fn bad_form(state: &AppState, viewer: &Viewer, err: FormRejection) -> Response {
    page(
        state,
        viewer,
        &PageQuery::default(),
        Outcome { status: Some(StatusCode::BAD_REQUEST), error: Some(err.body_text()), ..Outcome::default() },
    )
}

/// `GET /profiles?profile=&new=`.
pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Query(query) = query.unwrap_or_default();
    page(&state, &viewer, &query, Outcome::default())
}

/// `POST /profiles/{name}`: saves a profile over profiles.toml's.
pub async fn save(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(name): Path<String>,
    form: Result<Form<HashMap<String, String>>, FormRejection>,
) -> Response {
    let form = match form {
        Ok(Form(fields)) => ProfileForm::from_fields(&fields),
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
    form: Result<Form<HashMap<String, String>>, FormRejection>,
) -> Response {
    let form = match form {
        Ok(Form(fields)) => ProfileForm::from_fields(&fields),
        Err(err) => return bad_form(&state, &viewer, err),
    };
    let name = form.name.trim().to_owned();
    let error = if name.is_empty() {
        Some("the profile needs a name".to_owned())
    } else if state.live.profile_layers().effective().profiles.contains_key(&name) {
        Some(format!("there's a profile `{name}` already; open it to change it"))
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
