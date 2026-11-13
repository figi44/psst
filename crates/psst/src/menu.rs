//! What the quick menu asks the server, what its choices do there, and the profile it switches to.

use std::{io, path::Path, time::Duration};

use jiff::{Span, Timestamp, Zoned};
use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_proto::{
    DeviceId,
    api::{
        ApiErrorCode, HistoryRow, LoginLink, ModelState, ModelStatus, PostProcessingView, ProfilesView, Term,
        TermChange, encode_segment, routes,
    },
};
use psst_wayland::{MenuAction, ModelChoice};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::store::{read_json, write_json_atomic};

/// Facts the menu shows go stale on a slow link rather than leaving it waiting.
const FACTS_TIMEOUT: Duration = Duration::from_secs(5);
/// Unloading waits for the engine to let go of the GPU.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const PROFILE_FILE: &str = "profile.json";
/// As many as the history page shows.
const HISTORY_LIMIT: usize = 10;
const POST_PROCESSING_FILE: &str = "post_processing.json";

pub enum Fact {
    Profiles(Result<Vec<String>, String>),
    PostProcessing(Result<Vec<String>, String>),
    Model(ModelState),
    /// This machine's dictations that have text, newest first.
    History(Result<Vec<HistoryRow>, String>),
}

/// What became of a choice, for the overlay: what to say, if anything.
pub struct Done {
    pub result: Result<Option<String>, String>,
    /// The model's state after loading or unloading it.
    pub model: Option<ModelState>,
}

pub fn choice(state: ModelState) -> ModelChoice {
    match state {
        ModelState::Loaded | ModelState::Loading | ModelState::Downloading => ModelChoice::Unload,
        ModelState::Unloaded | ModelState::Failed | ModelState::Missing => ModelChoice::Load,
    }
}

/// All at once, each reported as it comes, so a slow one doesn't hold back the others.
pub fn fetch(api: ApiClient, device: Option<DeviceId>, report: impl Fn(Fact) + Clone + Send + 'static) {
    let (api2, report2) = (api.clone(), report.clone());
    let (api3, report3) = (api.clone(), report.clone());
    let (api4, report4) = (api.clone(), report.clone());
    tokio::spawn(async move {
        let styles = call::<PostProcessingView>(&api4, Method::Get, routes::POST_PROCESSING, FACTS_TIMEOUT).await;
        report4(Fact::PostProcessing(styles.map(|view| view.styles)));
    });
    tokio::spawn(async move {
        let profiles = call::<ProfilesView>(&api, Method::Get, routes::PROFILES, FACTS_TIMEOUT).await;
        report(Fact::Profiles(profiles.map(|view| view.profiles.into_iter().map(|p| p.name).collect())));
    });
    tokio::spawn(async move {
        if let Ok(status) = call::<ModelStatus>(&api2, Method::Get, routes::MODEL, FACTS_TIMEOUT).await {
            report2(Fact::Model(status.state));
        }
    });
    tokio::spawn(async move {
        let rows = match device {
            Some(device) => history(&api3, &device).await,
            None => Err("not connected to the server yet".into()),
        };
        report3(Fact::History(rows));
    });
}

async fn history(api: &ApiClient, device: &str) -> Result<Vec<HistoryRow>, String> {
    let path = format!("{}?limit={HISTORY_LIMIT}&device={}", routes::HISTORY, encode_segment(device));
    let rows: Vec<HistoryRow> = call(api, Method::Get, &path, FACTS_TIMEOUT).await?;
    // A server that predates `device` answers with every device's rows.
    let said = |row: &HistoryRow| row.r#final.as_ref().is_some_and(|f| !f.text.trim().is_empty());
    Ok(rows.into_iter().filter(|row| row.device == device && said(row)).take(HISTORY_LIMIT).collect())
}

/// When a dictation was said, as the history shows it: the time today, the day and time within
/// the week, else the date.
pub fn when(at: Timestamp, now: &Zoned) -> String {
    let at = at.to_zoned(now.time_zone().clone());
    let week_ago = now.date().checked_sub(Span::new().days(6)).unwrap_or(now.date());
    if at.date() == now.date() {
        at.strftime("%H:%M").to_string()
    } else if (week_ago..now.date()).contains(&at.date()) {
        at.strftime("%a %H:%M").to_string()
    } else {
        at.strftime("%b %-d").to_string()
    }
}

/// A recording's length, from its samples at 16 kHz.
pub fn length(samples: u64) -> String {
    let seconds = samples / u64::from(psst_proto::SAMPLE_RATE);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The choices that the server carries out.
pub async fn act(api: ApiClient, action: MenuAction) -> Done {
    match action {
        MenuAction::AddWord(word) => Done { result: add_word(&api, &word).await.map(Some), model: None },
        MenuAction::UnloadModel => model(&api, routes::MODEL_UNLOAD, |_| "Model unloaded").await,
        MenuAction::LoadModel => {
            model(&api, routes::MODEL_LOAD, |state| match state {
                ModelState::Loaded => "Model loaded",
                ModelState::Downloading => "Downloading the model\u{2026}",
                _ => "Loading the model\u{2026}",
            })
            .await
        }
        MenuAction::OpenWebUi => Done { result: open_web_ui(&api).await, model: None },
        MenuAction::SwitchProfile(_)
        | MenuAction::SwitchPostProcessing(_)
        | MenuAction::Paste(_)
        | MenuAction::Copy(_)
        | MenuAction::Discard(_) => unreachable!("done by the daemon"),
    }
}

async fn model(api: &ApiClient, route: &str, done: fn(ModelState) -> &'static str) -> Done {
    match call::<ModelStatus>(api, Method::Post, route, ACTION_TIMEOUT).await {
        Ok(ModelStatus { state, error: Some(error), .. }) => Done { result: Err(error), model: Some(state) },
        Ok(ModelStatus { state, .. }) => Done { result: Ok(Some(done(state).into())), model: Some(state) },
        Err(err) => Done { result: Err(err), model: None },
    }
}

async fn add_word(api: &ApiClient, word: &str) -> Result<String, String> {
    let term = Term {
        text: word.into(),
        sounds_like: Vec::new(),
        languages: "any".into(),
        fuzzy: true,
        pin: false,
        notes: None,
    };
    let add = api.call::<TermChange>(Method::Post, routes::DICTIONARY_TERMS, Some(&term));
    match tokio::time::timeout(ACTION_TIMEOUT, add).await {
        Ok(Ok(_)) => Ok(format!("Added \u{201c}{word}\u{201d}")),
        // The server's refusal names the API call that would replace it, which is no help here.
        Ok(Err(ApiCallError::Api { error, .. }))
            if error.code == ApiErrorCode::BadRequest && error.message.contains("already in the dictionary") =>
        {
            Ok(format!("\u{201c}{word}\u{201d} is already in the dictionary"))
        }
        Ok(Err(err)) => Err(describe(err)),
        Err(_) => Err("the server didn't answer in time".into()),
    }
}

async fn open_web_ui(api: &ApiClient) -> Result<Option<String>, String> {
    let link: LoginLink = call(api, Method::Post, routes::LOGIN_LINK, ACTION_TIMEOUT).await?;
    tokio::task::spawn_blocking(move || crate::web_ui::open(&link.url))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| format!("{err:#}"))?;
    Ok(None)
}

async fn call<T: serde::de::DeserializeOwned>(
    api: &ApiClient,
    method: Method,
    route: &str,
    timeout: Duration,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, api.call(method, route, None::<&()>)).await {
        Ok(result) => result.map_err(describe),
        Err(_) => Err("the server didn't answer in time".into()),
    }
}

/// Short enough for the pill; the log has the whole error.
fn describe(err: ApiCallError) -> String {
    warn!("menu: {err}");
    match err {
        ApiCallError::Api { error, .. } => error.message,
        ApiCallError::Connect { .. } => "cannot reach the server".into(),
        ApiCallError::Timeout(_) => "the server didn't answer in time".into(),
        ApiCallError::Protocol { .. } => "the server's answer made no sense; update psst?".into(),
    }
}

/// A choice made in the menu, kept until client.toml names another one.
#[derive(Debug, Serialize, Deserialize)]
struct Chosen {
    /// Named for the profile, the first choice kept this way.
    #[serde(rename = "profile")]
    choice: String,
    /// client.toml's value when it was chosen.
    instead_of: String,
}

pub fn load_profile(state_dir: &Path, configured: &str) -> Option<String> {
    load(&state_dir.join(PROFILE_FILE), "profile", configured)
}

pub fn save_profile(state_dir: &Path, configured: &str, profile: &str) -> io::Result<()> {
    save(&state_dir.join(PROFILE_FILE), configured, profile)
}

pub fn load_post_processing(state_dir: &Path, configured: &str) -> Option<String> {
    load(&state_dir.join(POST_PROCESSING_FILE), "post_processing", configured)
}

pub fn save_post_processing(state_dir: &Path, configured: &str, style: &str) -> io::Result<()> {
    save(&state_dir.join(POST_PROCESSING_FILE), configured, style)
}

fn load(path: &Path, key: &str, configured: &str) -> Option<String> {
    match read_json::<Chosen>(path) {
        Ok(chosen) if chosen.instead_of == configured => Some(chosen.choice),
        Ok(chosen) => {
            info!("client.toml's {key} is now {configured}, so it replaces {} from the menu", chosen.choice);
            let _ = std::fs::remove_file(path);
            None
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!("ignoring {}: {err}", path.display());
            None
        }
    }
}

fn save(path: &Path, configured: &str, choice: &str) -> io::Result<()> {
    if choice == configured {
        return match std::fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    write_json_atomic(path, &Chosen { choice: choice.into(), instead_of: configured.into() }, 0o600)
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    #[test]
    fn when_says_the_time_today_the_day_this_week_and_else_the_date() {
        let now = "2026-11-10T15:30:00Z".parse::<Timestamp>().unwrap().to_zoned(TimeZone::UTC);
        let at = |ts: &str| when(ts.parse().unwrap(), &now);
        assert_eq!(at("2026-11-10T08:05:00Z"), "08:05");
        assert_eq!(at("2026-11-09T23:59:00Z"), "Mon 23:59");
        assert_eq!(at("2026-11-04T09:00:00Z"), "Wed 09:00");
        assert_eq!(at("2026-11-03T09:00:00Z"), "Nov 3");
        assert_eq!(at("2026-11-11T09:00:00Z"), "Nov 11", "a clock ahead of this one");
    }

    #[test]
    fn length_counts_minutes_and_seconds() {
        assert_eq!(length(0), "0:00");
        assert_eq!(length(34 * 16_000 + 15_999), "0:34");
        assert_eq!(length(61 * 16_000), "1:01");
    }
}
