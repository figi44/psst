//! Post-processing: an LLM cleans up dictated text on the user's ChatGPT plan, in the style the
//! client asks for. The web UI's lab tries it out; its state lives in memory, shared by everyone
//! using the web UI, while edited styles are kept in the database.

pub mod chatgpt;
pub mod diff;
mod https;
pub mod prompts;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use jiff::Timestamp;
use psst_proto::Final;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use chatgpt::{Answer, ApiError, Ask, Model, Pending};
use https::Https;
use prompts::Variant;

const KEPT_RUNS: usize = 50;

#[derive(Debug)]
pub struct Lab {
    https: Https,
    pending: Mutex<Option<Pending>>,
    refresh: tokio::sync::Mutex<()>,
    models: Mutex<Option<(Instant, Vec<Model>)>>,
    runs: Mutex<VecDeque<Run>>,
    draft: Mutex<Draft>,
    socket: tokio::sync::Mutex<Option<chatgpt::Socket>>,
}

impl Default for Lab {
    fn default() -> Lab {
        Lab {
            https: Https::new(),
            pending: Mutex::default(),
            refresh: tokio::sync::Mutex::default(),
            models: Mutex::default(),
            runs: Mutex::default(),
            draft: Mutex::default(),
            socket: tokio::sync::Mutex::default(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lab {
    pub fn pending(&self) -> MutexGuard<'_, Option<Pending>> {
        lock(&self.pending)
    }

    fn models(&self) -> MutexGuard<'_, Option<(Instant, Vec<Model>)>> {
        lock(&self.models)
    }

    pub fn draft(&self) -> Draft {
        lock(&self.draft).clone()
    }

    pub fn set_draft(&self, draft: Draft) {
        *lock(&self.draft) = draft;
    }

    pub fn runs(&self) -> Vec<Run> {
        lock(&self.runs).iter().cloned().collect()
    }

    pub fn clear_runs(&self) {
        lock(&self.runs).clear();
    }
}

/// What the lab's form holds between runs.
#[derive(Debug, Clone)]
pub struct Draft {
    pub input: String,
    pub variant: String,
}

impl Default for Draft {
    fn default() -> Draft {
        Draft { input: String::new(), variant: prompts::DEFAULT.to_owned() }
    }
}

/// A post-processing style as dictations use it: built in, or as saved in the web UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    pub prompt: String,
    pub model: String,
    /// `default` sends none.
    pub effort: String,
    /// `default` sends none.
    pub tier: String,
}

impl Style {
    pub fn built_in(variant: &Variant) -> Style {
        Style {
            prompt: variant.text().to_owned(),
            model: variant.model.to_owned(),
            effort: variant.effort.to_owned(),
            tier: variant.tier.to_owned(),
        }
    }
}

/// A style the web UI shows and dictations can ask for: built in, or one the user made.
#[derive(Debug, Clone)]
pub struct Entry {
    /// What clients send; a custom style's is its name.
    pub id: String,
    pub name: String,
    pub built_in: Option<&'static Variant>,
    pub style: Style,
    /// A built-in style saved with changes.
    pub edited: bool,
}

const CUSTOM_KEY: &str = "post_processing.custom";
const MAX_NAME: usize = 32;

fn style_key(id: &str) -> String {
    format!("post_processing.style.{id}")
}

fn saved_style(state: &AppState, id: &str) -> Option<Style> {
    state.db.get_state(&style_key(id)).ok().flatten().and_then(|text| serde_json::from_str(&text).ok())
}

/// In the order they were made.
fn custom_ids(state: &AppState) -> Vec<String> {
    state.db.get_state(CUSTOM_KEY).ok().flatten().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn set_custom_ids(state: &AppState, ids: &[String]) -> anyhow::Result<()> {
    state.db.set_state(CUSTOM_KEY, &serde_json::to_string(ids)?)
}

/// Built-in styles first, then the custom ones.
pub fn entries(state: &AppState) -> Vec<Entry> {
    let built_in = prompts::VARIANTS.iter().map(|variant| {
        let saved = saved_style(state, variant.id);
        Entry {
            id: variant.id.to_owned(),
            name: variant.name.to_owned(),
            built_in: Some(variant),
            edited: saved.is_some(),
            style: saved.unwrap_or_else(|| Style::built_in(variant)),
        }
    });
    let custom = custom_ids(state).into_iter().filter_map(|id| {
        let style = saved_style(state, &id)?;
        Some(Entry { name: id.clone(), id, built_in: None, edited: false, style })
    });
    built_in.chain(custom).collect()
}

pub fn entry(state: &AppState, id: &str) -> Option<Entry> {
    entries(state).into_iter().find(|entry| entry.id == id)
}

/// A built-in style saved as it was built returns to following the built-in one.
pub fn save_style(state: &AppState, entry: &Entry, style: &Style) -> anyhow::Result<()> {
    if entry.built_in.is_some_and(|variant| *style == Style::built_in(variant)) {
        return state.db.clear_state(&style_key(&entry.id));
    }
    state.db.set_state(&style_key(&entry.id), &serde_json::to_string(style)?)
}

pub fn reset_style(state: &AppState, entry: &Entry) -> anyhow::Result<()> {
    if entry.built_in.is_some() {
        state.db.clear_state(&style_key(&entry.id))?;
    }
    Ok(())
}

/// A new custom style named `name`, starting as a copy of `from`. The error says what's wrong
/// with the name.
pub fn create_style(state: &AppState, name: &str, from: &Style) -> Result<String, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Give the new style a name.".into());
    }
    if name.chars().count() > MAX_NAME {
        return Err(format!("Keep the name under {MAX_NAME} characters."));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_')) {
        return Err("Use letters, digits, spaces, - and _ in the name.".into());
    }
    let taken = |id: &str| id.eq_ignore_ascii_case(&name);
    if taken("none") || entries(state).iter().any(|entry| taken(&entry.id) || taken(&entry.name)) {
        return Err(format!("There's already a style called {name}."));
    }
    let mut ids = custom_ids(state);
    ids.push(name.clone());
    state
        .db
        .set_state(&style_key(&name), &serde_json::to_string(from).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    set_custom_ids(state, &ids).map_err(|e| e.to_string())?;
    Ok(name)
}

pub fn delete_style(state: &AppState, entry: &Entry) -> anyhow::Result<()> {
    if entry.built_in.is_some() {
        return Ok(());
    }
    let ids: Vec<String> = custom_ids(state).into_iter().filter(|id| *id != entry.id).collect();
    set_custom_ids(state, &ids)?;
    state.db.clear_state(&style_key(&entry.id))
}

#[derive(Debug, Clone)]
pub struct Run {
    pub at: Timestamp,
    pub input: String,
    pub variant: String,
    pub edited: bool,
    pub instructions: String,
    pub model: String,
    pub effort: String,
    pub tier: String,
    pub outcome: Result<Answer, Failure>,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub text: String,
    pub usage_limit: bool,
}

/// The prompt as sent: the style's, with the dictionary's words.
fn instructions(state: &AppState, style: &Style) -> String {
    let words: Vec<String> = state.live.dictionary().terms().iter().map(|t| t.text.clone()).collect();
    prompts::with_vocabulary(&style.prompt, &words)
}

fn ask<'a>(style: &'a Style, instructions: &'a str, text: &'a str) -> Ask<'a> {
    let sent = |v: &'a str| (v != "default").then_some(v);
    Ask { model: &style.model, effort: sent(&style.effort), tier: sent(&style.tier), instructions, text, extra: None }
}

/// The client gives up on a dictation 10 s after it stops; past this, it gets the text as it was.
const DICTATION_BUDGET: Duration = Duration::from_secs(6);

/// Post-processes a dictation's final text with `style_id`, keeping the text as it was when that
/// fails or takes too long.
pub async fn dictation(state: &Arc<AppState>, style_id: &str, fin: &mut Final) {
    let Some(Entry { style, .. }) = entry(state, style_id) else {
        tracing::warn!("unknown post-processing style {style_id:?}; the dictation stays as it was");
        return;
    };
    let text = fin.text.trim();
    if text.is_empty() {
        return;
    }
    let instructions = instructions(state, &style);
    let started = Instant::now();
    match tokio::time::timeout(DICTATION_BUDGET, chatgpt::respond(state, &ask(&style, &instructions, text))).await {
        Ok(Ok(answer)) => {
            tracing::info!(
                "post-processed a dictation with {style_id} in {} ms (first token after {} ms)",
                started.elapsed().as_millis(),
                answer.first_token.map_or(0, |d| d.as_millis())
            );
            // The rules' trailing space keeps the next dictation from running into this one.
            let trailing = &fin.text[fin.text.trim_end().len()..];
            fin.text = format!("{}{trailing}", answer.text);
        }
        Ok(Err(err)) => {
            tracing::warn!("post-processing a dictation with {style_id} failed, so it stays as it was: {err:#}")
        }
        Err(_) => tracing::warn!("post-processing a dictation took over {DICTATION_BUDGET:?}, so it stays as it was"),
    }
}

/// Runs `input` through a style as dictations would, and keeps the run for the page.
pub async fn run(state: &Arc<AppState>, input: &str, entry: &Entry) -> Run {
    let Entry { style, edited, .. } = entry.clone();
    let instructions = instructions(state, &style);
    let outcome = chatgpt::respond(state, &ask(&style, &instructions, input)).await.map_err(|err| Failure {
        usage_limit: err.downcast_ref::<ApiError>().is_some_and(ApiError::usage_limit),
        text: format!("{err:#}"),
    });
    let run = Run {
        at: Timestamp::now(),
        input: input.to_owned(),
        variant: entry.name.clone(),
        edited,
        instructions,
        model: style.model,
        effort: style.effort,
        tier: style.tier,
        outcome,
    };
    let mut runs = lock(&state.tidy.runs);
    runs.push_front(run.clone());
    runs.truncate(KEPT_RUNS);
    run
}
