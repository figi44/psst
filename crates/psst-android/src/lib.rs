//! The Android app's dictation logic, platform-neutral: commands and audio go in, events come out.
//! The app drives one [`Core`] per process over JNI (`ffi`); the tests drive it directly.

mod actor;
mod config;
mod connect;
mod ffi;
mod history;
mod logcat;

use std::{
    path::PathBuf,
    sync::{Mutex, PoisonError, mpsc as std_mpsc},
    thread,
    time::Duration,
};

pub use psst_proto::SessionId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::error;

use crate::actor::Input;

/// The JSON the app sends is this, tagged by `type`. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Persisted. A bare `host` or `host:port` gets `http://`, and `""` forgets the server. A
    /// different server forgets the old one's credentials.
    Configure {
        server: String,
    },
    Pair {
        code: String,
    },
    /// The UI is on screen: connect, or check the connection is alive.
    Warm,
    /// Ignored unless the dictation is `idle` and a server is configured.
    Start,
    Stop,
    Cancel,
    /// Deletes a saved dictation.
    Discard {
        session_id: SessionId,
    },
    /// The app put that dictation's text into a field: a final's, or a saved dictation's, which
    /// is then deleted.
    Inserted {
        session_id: SessionId,
    },
    /// This phone's recent dictations, from the server. Answered by one `history` event.
    History,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Whenever any field changes, and once at start.
    State { state: State },
    /// Once per delivered dictation, followed by a state whose dictation is `idle`. `text` is `""`
    /// when nothing was heard.
    Final { session_id: SessionId, text: String },
    /// Once per dictation the server couldn't take in time, followed by a state whose dictation is
    /// `idle` and whose `saved` has it. It's sent in the background, never delivered as a final.
    Saved { session_id: SessionId },
    /// Once per `history` command: at most 20, newest first, and none when `error` says why.
    History { items: Vec<HistoryItem>, error: Option<String> },
}

/// A dictation of this phone's that has text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub session_id: SessionId,
    pub text: String,
    /// When it started, in Unix milliseconds.
    pub created_ms: i64,
}

/// A dictation that never went in, kept on the phone until it's inserted or discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDictation {
    pub session_id: SessionId,
    /// When it started, in Unix milliseconds.
    pub created_ms: i64,
    pub duration_ms: u64,
    /// `None` until the server has transcribed it.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub server: Option<String>,
    pub server_version: Option<String>,
    pub link: Link,
    pub link_message: Option<String>,
    pub dictation: Dictation,
    pub dictation_message: Option<String>,
    pub seconds_left: Option<u32>,
    /// Newest first.
    pub saved: Vec<SavedDictation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Unconfigured,
    Connecting,
    Online,
    Offline,
    /// The server refused this phone and pairing by tailnet identity didn't work.
    NeedsPairing,
    UpdateApp,
    UpdateServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dictation {
    Idle,
    /// The app records exactly while the dictation is in this state.
    Recording,
    Transcribing,
    LoadingModel,
    /// Re-delivering the recording after a lost connection.
    Sending,
    /// Cannot reach the server; still trying, until the dictation is saved.
    WaitingForServer,
}

/// The desktop daemon's budgets where they apply; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// For a connection made on `configure`, `warm`, `pair` or `start`.
    pub connect_budget: Duration,
    /// How long a recording waits for the connection before it says it is offline. Grows with the
    /// round trip.
    pub start_budget: Duration,
    /// The wait for a pong when checking that a quiet connection is alive. Grows with the round
    /// trip.
    pub pong: Duration,
    /// How long after `stop` the core keeps trying to reach the server before the dictation is
    /// saved.
    pub redeliver_window: Duration,
    pub attempt_budget: Duration,
    /// Doubles after each failed attempt, up to 2 s.
    pub first_backoff: Duration,
    /// For fetching the recent dictations.
    pub history_budget: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            connect_budget: Duration::from_secs(5),
            start_budget: Duration::from_millis(700),
            pong: Duration::from_millis(300),
            redeliver_window: Duration::from_secs(10),
            attempt_budget: Duration::from_secs(2),
            first_backoff: Duration::from_millis(250),
            history_budget: Duration::from_secs(5),
        }
    }
}

/// Runs on its own thread with a single-threaded runtime. Dropping it stops it.
pub struct Core {
    input: mpsc::UnboundedSender<Input>,
    events: Mutex<std_mpsc::Receiver<Event>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Core {
    /// `data_dir` keeps the server, device id and token, and the recordings. `device_name` names
    /// the phone when it redeems a pairing code.
    pub fn start(data_dir: impl Into<PathBuf>, device_name: impl Into<String>) -> Core {
        Core::with_timing(data_dir, device_name, Timing::default())
    }

    pub fn with_timing(data_dir: impl Into<PathBuf>, device_name: impl Into<String>, timing: Timing) -> Core {
        let (input, input_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = std_mpsc::channel();
        let (data_dir, device_name) = (data_dir.into(), device_name.into());
        let thread = thread::Builder::new()
            .name("psst-core".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(err) => return error!("cannot start the core's runtime: {err}"),
                };
                runtime.block_on(actor::run(data_dir, device_name, timing, input_rx, events_tx));
            })
            .inspect_err(|err| error!("cannot start the core's thread: {err}"))
            .ok();
        Core { input, events: Mutex::new(events), thread }
    }

    /// Never blocks. Commands and audio are handled in the order they are given.
    pub fn send(&self, command: Command) {
        let _ = self.input.send(Input::Command(command));
    }

    /// Samples of the recording in progress: 16 kHz mono, any chunk size. Never blocks.
    pub fn audio(&self, samples: impl Into<Vec<i16>>) {
        let _ = self.input.send(Input::Audio(samples.into()));
    }

    /// Blocks until there is an event; `None` once the core has stopped.
    pub fn next_event(&self) -> Option<Event> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).recv().ok()
    }

    pub fn next_event_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).recv_timeout(timeout).ok()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.input.send(Input::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
