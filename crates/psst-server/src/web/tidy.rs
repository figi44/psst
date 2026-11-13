use std::sync::Arc;

use axum::{
    Form, Json,
    extract::{
        Path, Query, State,
        rejection::{FormRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::ORIGIN},
    response::{IntoResponse, Redirect, Response},
};
use jiff::tz::TimeZone;
use minijinja::{Value, context, value::Serde};
use psst_proto::{HistoryId, api::encode_segment};
use serde::Deserialize;
use serde_json::json;

use super::{Viewer, chrome, render};
use crate::{
    state::AppState,
    tidy::{self, Draft, Run, Style, chatgpt, diff, prompts},
};

type AppRef = Arc<AppState>;

const RECENT: u32 = 10;
const EFFORTS: [(&str, &str); 7] = [
    ("none", "None"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
    ("default", "Not sent"),
];
/// Fast's ID on the wire is `priority`: `fast` is refused, and responses report `default` either way.
const TIERS: [(&str, &str); 2] = [("priority", "Fast"), ("default", "Standard")];

const PAGE: &str = "/post-processing";

#[derive(Debug, Default, Deserialize)]
pub struct LabForm {
    #[serde(default)]
    input: String,
    #[serde(default)]
    variant: String,
    #[serde(default)]
    app: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    example: String,
    #[serde(default)]
    load: String,
    #[serde(default)]
    action: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct StyleForm {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
    #[serde(default)]
    tier: String,
    #[serde(default)]
    action: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct PasteForm {
    #[serde(default)]
    url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    style: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NewStyleForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    from: String,
}

fn page_of(style: &str) -> String {
    format!("{PAGE}?style={}", encode_segment(style))
}

/// `?style=` picks the style the page shows, and the lab runs.
pub async fn show(
    viewer: Viewer,
    State(state): State<AppRef>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    if let Ok(Query(PageQuery { style: Some(id) })) = query
        && tidy::entry(&state, &id).is_some()
    {
        state.tidy.set_draft(Draft { variant: id, ..state.tidy.draft() });
    }
    page(&state, &viewer, None).await
}

pub async fn act(viewer: Viewer, State(state): State<AppRef>, form: Result<Form<LabForm>, FormRejection>) -> Response {
    let Ok(Form(form)) = form else { return Redirect::to(PAGE).into_response() };
    let mut draft = Draft {
        input: form.input.replace("\r\n", "\n").trim().to_owned(),
        variant: form.variant,
        app: form.app,
        title: form.title,
    };
    if let Some(id) = form.load.strip_prefix("h:").and_then(|id| id.parse::<HistoryId>().ok())
        && let Ok(Some(row)) = state.db.history_row(id)
        && let Some(fin) = row.r#final
    {
        draft.input = fin.text.trim().to_owned();
    }
    match form.action.as_str() {
        "example" => {
            if let Some(example) = prompts::examples().iter().find(|e| e.id == form.example) {
                draft.input = example.text.clone();
            }
        }
        "run" | "all" if draft.input.is_empty() => {
            state.tidy.set_draft(draft);
            return page(&state, &viewer, Some("Type or load a transcript first.".into())).await;
        }
        "run" => {
            if let Some(entry) = tidy::entry(&state, &draft.variant) {
                tidy::run(&state, &draft.input, draft.target().as_ref(), &entry).await;
            }
        }
        "all" => {
            // Newest runs show first, so the last one run tops the list.
            for entry in tidy::entries(&state).iter().rev() {
                tidy::run(&state, &draft.input, draft.target().as_ref(), entry).await;
            }
        }
        _ => {}
    }
    let back = page_of(&draft.variant);
    state.tidy.set_draft(draft);
    Redirect::to(&back).into_response()
}

pub async fn save_style(
    viewer: Viewer,
    State(state): State<AppRef>,
    Path(id): Path<String>,
    form: Result<Form<StyleForm>, FormRejection>,
) -> Response {
    let (Some(entry), Ok(Form(form))) = (tidy::entry(&state, &id), form) else {
        return Redirect::to(PAGE).into_response();
    };
    let saved = match form.action.as_str() {
        "reset" => tidy::reset_style(&state, &entry),
        "delete" => tidy::delete_style(&state, &entry),
        _ => {
            let prompt = form.prompt.replace("\r\n", "\n").trim().to_owned();
            let model = form.model.trim().to_owned();
            let known = |value: &str, list: &[(&str, &str)]| list.iter().any(|(id, _)| *id == value);
            if prompt.is_empty() || model.is_empty() {
                return page(
                    &state,
                    &viewer,
                    Some(format!("{}: the prompt and the model can't be empty.", entry.name)),
                )
                .await;
            }
            if !known(&form.effort, &EFFORTS) || !known(&form.tier, &TIERS) {
                return Redirect::to(PAGE).into_response();
            }
            tidy::save_style(&state, &entry, &Style { prompt, model, effort: form.effort, tier: form.tier })
        }
    };
    if let Err(err) = saved {
        return page(&state, &viewer, Some(format!("Couldn't change {}: {err:#}", entry.name))).await;
    }
    let shown = if form.action == "delete" { prompts::DEFAULT } else { entry.id.as_str() };
    Redirect::to(&page_of(shown)).into_response()
}

pub async fn create_style(
    viewer: Viewer,
    State(state): State<AppRef>,
    form: Result<Form<NewStyleForm>, FormRejection>,
) -> Response {
    let Ok(Form(form)) = form else { return Redirect::to(PAGE).into_response() };
    let from = tidy::entry(&state, &form.from).map_or_else(|| tidy::Style::built_in(prompts::default()), |e| e.style);
    match tidy::create_style(&state, &form.name, &from) {
        Ok(id) => Redirect::to(&page_of(&id)).into_response(),
        Err(why) => page(&state, &viewer, Some(why)).await,
    }
}

pub async fn connect(viewer: Viewer, State(state): State<AppRef>, headers: HeaderMap) -> Response {
    // The viewer's request passed the same-origin check, so its Origin is psst's own.
    let back = headers
        .get(ORIGIN)
        .and_then(|origin| origin.to_str().ok())
        .filter(|origin| origin.starts_with("http://") || origin.starts_with("https://"))
        .map(|origin| format!("{origin}{PAGE}"));
    match chatgpt::start(&state, back).await {
        Ok(()) => Redirect::to(PAGE).into_response(),
        Err(err) => page(&state, &viewer, Some(format!("Couldn't start the sign-in: {err:#}"))).await,
    }
}

pub async fn paste(
    viewer: Viewer,
    State(state): State<AppRef>,
    form: Result<Form<PasteForm>, FormRejection>,
) -> Response {
    let url = form.map(|Form(form)| form.url).unwrap_or_default();
    match chatgpt::paste(&state, &url).await {
        Ok(_) => Redirect::to(PAGE).into_response(),
        Err(err) => page(&state, &viewer, Some(format!("{err:#}"))).await,
    }
}

pub async fn cancel(_viewer: Viewer, State(state): State<AppRef>) -> Response {
    chatgpt::cancel(&state);
    Redirect::to(PAGE).into_response()
}

pub async fn disconnect(viewer: Viewer, State(state): State<AppRef>) -> Response {
    match chatgpt::disconnect(&state).await {
        Ok(()) => Redirect::to(PAGE).into_response(),
        Err(err) => page(&state, &viewer, Some(format!("Couldn't disconnect: {err:#}"))).await,
    }
}

pub async fn clear(_viewer: Viewer, State(state): State<AppRef>) -> Response {
    state.tidy.clear_runs();
    Redirect::to(PAGE).into_response()
}

/// One request with any instructions, for scripted evals. It leaves the page's runs alone.
#[derive(Debug, Deserialize)]
pub struct EvalRequest {
    model: String,
    effort: Option<String>,
    tier: Option<String>,
    instructions: String,
    input: String,
    extra: Option<serde_json::Value>,
}

pub async fn eval(_viewer: Viewer, State(state): State<AppRef>, Json(req): Json<EvalRequest>) -> Response {
    let ask = chatgpt::Ask {
        model: &req.model,
        effort: req.effort.as_deref(),
        tier: req.tier.as_deref(),
        instructions: &req.instructions,
        text: &req.input,
        extra: req.extra.as_ref(),
    };
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let body = match chatgpt::respond(&state, &ask).await {
        Ok(answer) => json!({
            "ok": true,
            "text": answer.text,
            "first_ms": answer.first_token.map(ms),
            "total_ms": ms(answer.total),
            "connect_ms": answer.connect.map(ms),
            "input_tokens": answer.input_tokens,
            "output_tokens": answer.output_tokens,
            "reasoning_tokens": answer.reasoning_tokens,
        }),
        Err(err) => json!({
            "ok": false,
            "error": format!("{err:#}"),
            "code": err.downcast_ref::<chatgpt::ApiError>().map(|e| e.code.clone()),
        }),
    };
    Json(body).into_response()
}

pub async fn eval_models(_viewer: Viewer, State(state): State<AppRef>) -> Response {
    match chatgpt::models(&state).await {
        Ok(models) => Json(json!({ "ok": true, "models": models })).into_response(),
        Err(err) => Json(json!({ "ok": false, "error": format!("{err:#}") })).into_response(),
    }
}

async fn page(state: &AppRef, viewer: &Viewer, error: Option<String>) -> Response {
    let draft = state.tidy.draft();
    let email = chatgpt::credentials(state).map(|c| c.email);
    if email.is_some() {
        chatgpt::warm(state);
    }
    let pending =
        state.tidy.sign_in().pending.as_ref().map(|p| context! { url => p.url.clone(), listening => p.listening });
    let (models, models_error) = match &email {
        Some(_) => match chatgpt::models(state).await {
            Ok(models) => (models, None),
            Err(err) => (Vec::new(), Some(format!("{err:#}"))),
        },
        None => (Vec::new(), None),
    };

    let entries = tidy::entries(state);
    let current = entries.iter().find(|e| e.id == draft.variant).unwrap_or(&entries[0]);
    let switch: Vec<Value> = entries
        .iter()
        .map(|e| {
            context! {
                name => e.name.clone(),
                href => page_of(&e.id),
                current => e.id == current.id,
                changed => e.edited,
            }
        })
        .collect();
    let options = |list: &[(&str, &str)], current: &str| -> Vec<Value> {
        list.iter().map(|&(id, label)| context! { id, label, selected => id == current }).collect()
    };
    let default = current.built_in.map(tidy::Style::built_in);
    let style = context! {
        id => current.id.clone(),
        name => current.name.clone(),
        action => format!("{PAGE}/styles/{}", encode_segment(&current.id)),
        about => current.built_in.map(|v| v.about),
        custom => current.built_in.is_none(),
        edited => current.edited,
        prompt => current.style.prompt.clone(),
        model => current.style.model.clone(),
        effort => current.style.effort.clone(),
        tier => current.style.tier.clone(),
        efforts => options(&EFFORTS, &current.style.effort),
        tiers => options(&TIERS, &current.style.tier),
        default => default.map(|d| context! { prompt => d.prompt, model => d.model, effort => d.effort, tier => d.tier }),
    };

    let recent: Vec<Value> = state
        .db
        .history(RECENT, None)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let text = row.r#final?.text.trim().to_owned();
            (!text.is_empty()).then(|| context! { id => row.id, text => shorten(&text, 110) })
        })
        .collect();
    let examples: Vec<Value> =
        prompts::examples().iter().map(|e| context! { id => e.id.clone(), text => shorten(&e.text, 70) }).collect();

    let tz = TimeZone::system();
    let runs: Vec<Value> = state.tidy.runs().iter().map(|run| view(run, &tz)).collect();
    let status = if error.is_some() { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::OK };
    let ctx = context! {
        error,
        email,
        pending,
        manage_usage => chatgpt::MANAGE_USAGE,
        input => draft.input.clone(),
        app => draft.app.clone(),
        title => draft.title.clone(),
        switch,
        style,
        styles_count => entries.len(),
        examples_count => prompts::examples().len(),
        models => Value::from(Serde(models)),
        models_error,
        words => state.live.dictionary().terms().len(),
        recent,
        examples,
        runs,
        ..chrome(state, viewer, "post-processing")
    };
    render(status, "post-processing.html", ctx)
}

fn view(run: &Run, tz: &TimeZone) -> Value {
    let time = run.at.to_zoned(tz.clone()).strftime("%H:%M:%S").to_string();
    let tier = TIERS.iter().find(|(id, _)| *id == run.tier).map_or(run.tier.as_str(), |(_, label)| label);
    let effort = EFFORTS.iter().find(|(id, _)| *id == run.effort).map_or(run.effort.as_str(), |(_, label)| label);
    let base = context! {
        variant => run.variant.clone(),
        edited => run.edited,
        model => run.model.clone(),
        effort => effort.to_lowercase(),
        tier,
        time,
        target => run.target.as_ref().map(|t| match &t.title {
            Some(title) => format!("{} · {}", t.app, shorten(title, 48)),
            None => t.app.clone(),
        }),
        input => run.input.clone(),
        instructions => run.instructions.clone(),
    };
    let outcome = match &run.outcome {
        Ok(answer) => {
            let ms = |d: std::time::Duration| d.as_millis().to_string();
            context! {
                ok => true,
                text => answer.text.clone(),
                segments => diff::words(&run.input, &answer.text).map(|s| Value::from(Serde(s))),
                total_ms => ms(answer.total),
                first_ms => answer.first_token.map_or("–".into(), |d| format!("{} ms", ms(d))),
                tokens => format!(
                    "{} in · {} out · {} reasoning",
                    answer.input_tokens, answer.output_tokens, answer.reasoning_tokens
                ),
                connect_ms => answer.connect.map(ms),
            }
        }
        Err(failure) => context! { ok => false, error => failure.text.clone(), usage_limit => failure.usage_limit },
    };
    context! { ..base, ..outcome }
}

fn shorten(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}
