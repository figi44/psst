//! What the quick menu asks the server, what its choices do there, and the profile it switches to.

use std::{io, path::Path, time::Duration};

use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_proto::api::{ApiErrorCode, LoginLink, ModelState, ModelStatus, ProfilesView, Term, TermChange, routes};
use psst_wayland::{MenuAction, ModelChoice};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::store::{read_json, write_json_atomic};

/// Facts the menu shows go stale on a slow link rather than leaving it waiting.
const FACTS_TIMEOUT: Duration = Duration::from_secs(5);
/// Unloading waits for the engine to let go of the GPU.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const PROFILE_FILE: &str = "profile.json";

pub enum Fact {
    Profiles(Result<Vec<String>, String>),
    Model(ModelState),
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

/// Both at once, each reported as it comes, so a slow one doesn't hold back the other.
pub fn fetch(api: ApiClient, report: impl Fn(Fact) + Clone + Send + 'static) {
    let (api2, report2) = (api.clone(), report.clone());
    tokio::spawn(async move {
        let profiles = call::<ProfilesView>(&api, Method::Get, routes::PROFILES, FACTS_TIMEOUT).await;
        report(Fact::Profiles(profiles.map(|view| view.profiles.into_iter().map(|p| p.name).collect())));
    });
    tokio::spawn(async move {
        if let Ok(status) = call::<ModelStatus>(&api2, Method::Get, routes::MODEL, FACTS_TIMEOUT).await {
            report2(Fact::Model(status.state));
        }
    });
}

/// Every choice but switching the profile, which never leaves this machine.
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
        MenuAction::SwitchProfile(_) => unreachable!("switched by the daemon"),
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

/// The profile chosen in the menu, kept until client.toml names another one.
#[derive(Debug, Serialize, Deserialize)]
struct Chosen {
    profile: String,
    /// client.toml's profile when it was chosen.
    instead_of: String,
}

pub fn load_profile(state_dir: &Path, configured: &str) -> Option<String> {
    let path = state_dir.join(PROFILE_FILE);
    match read_json::<Chosen>(&path) {
        Ok(chosen) if chosen.instead_of == configured => Some(chosen.profile),
        Ok(chosen) => {
            info!("client.toml's profile is now {configured}, so it replaces {} from the menu", chosen.profile);
            let _ = std::fs::remove_file(&path);
            None
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!("ignoring {}: {err}", path.display());
            None
        }
    }
}

pub fn save_profile(state_dir: &Path, configured: &str, profile: &str) -> io::Result<()> {
    let path = state_dir.join(PROFILE_FILE);
    if profile == configured {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    write_json_atomic(&path, &Chosen { profile: profile.into(), instead_of: configured.into() }, 0o600)
}
