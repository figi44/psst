//! Whoever moves a session to an outcome, under the one lock, tells the attached connection, so
//! each session ends with exactly one terminal message.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use psst_proto::{
    DeviceId, ErrorCode, HistoryId, SessionId,
    ws::{ErrorMsg, FinalMsg, ServerMsg},
};
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};

use crate::{
    db::Db,
    engine::{JobHandle, JobId, JobStatus},
};

pub const FINISHED_TTL: Duration = Duration::from_secs(120);
pub const ORPHAN_RECORDING_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub device: DeviceId,
    pub session_id: SessionId,
}

#[derive(Debug)]
pub enum ConnEvent {
    Msg(ServerMsg),
    Detached { session_id: SessionId },
    Close { reason: String },
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub conn_id: u64,
    pub tx: mpsc::UnboundedSender<ConnEvent>,
}

impl Attachment {
    fn send(&self, event: ConnEvent) {
        let _ = self.tx.send(event);
    }
}

#[derive(Debug)]
struct Entry {
    /// Changes whenever the session starts over, so stale holders can't act on it.
    epoch: u64,
    state: State,
    attached: Option<Attachment>,
}

#[derive(Debug)]
enum State {
    Recording,
    Processing { job_id: JobId, status: Arc<JobStatus>, audio_ms: u32 },
    Finished { msg: Arc<FinalMsg>, expires: Instant },
}

#[derive(Debug, Clone)]
pub struct Rejection {
    pub code: ErrorCode,
    pub message: String,
    pub detail: Option<Value>,
}

impl Rejection {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), detail: None }
    }

    pub fn into_msg(self, session_id: SessionId) -> ErrorMsg {
        ErrorMsg { detail: self.detail, ..ErrorMsg::new(Some(session_id), self.code, self.message) }
    }
}

#[derive(Debug)]
pub enum Start {
    Record { epoch: u64, history_id: HistoryId },
    Finished(Arc<FinalMsg>),
    Attached { epoch: u64, status: Arc<JobStatus>, audio_ms: u32 },
    Rejected(Rejection),
}

pub struct StartRequest<'a> {
    pub key: SessionKey,
    pub resume: bool,
    pub profile: &'a str,
    pub conn: Attachment,
    /// Checked after the resume lookups: only a new recording needs it.
    pub admit: &'a dyn Fn() -> Result<(), Rejection>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cancel {
    Cancelled(Option<JobId>),
    Ignored,
}

#[derive(Debug)]
pub struct Registry {
    sessions: Mutex<HashMap<SessionKey, Entry>>,
    next_epoch: AtomicU64,
    max_active: usize,
}

impl Registry {
    pub fn new(max_active: usize) -> Arc<Registry> {
        Arc::new(Registry { sessions: Mutex::default(), next_epoch: AtomicU64::new(1), max_active })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Entry>> {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn epoch(&self) -> u64 {
        self.next_epoch.fetch_add(1, Ordering::Relaxed)
    }

    pub fn start(&self, db: &Db, req: StartRequest<'_>) -> Result<Start> {
        let mut sessions = self.lock();
        let reused =
            || Rejection::new(ErrorCode::BadRequest, "this session_id was already used; resume it with `resume: true`");

        enum Existing {
            Finished(Arc<FinalMsg>),
            Processing { epoch: u64, status: Arc<JobStatus>, audio_ms: u32 },
            Recording,
        }
        let existing = sessions.get(&req.key).map(|entry| match &entry.state {
            State::Finished { msg, .. } => Existing::Finished(msg.clone()),
            State::Processing { status, audio_ms, .. } => {
                Existing::Processing { epoch: entry.epoch, status: status.clone(), audio_ms: *audio_ms }
            }
            State::Recording => Existing::Recording,
        });
        match existing {
            Some(_) if !req.resume => return Ok(Start::Rejected(reused())),
            Some(Existing::Finished(msg)) => return Ok(Start::Finished(msg)),
            Some(Existing::Processing { epoch, status, audio_ms }) => {
                let entry = sessions.get_mut(&req.key).expect("entry exists");
                if let Some(old) = entry.attached.replace(req.conn.clone())
                    && old.conn_id != req.conn.conn_id
                {
                    old.send(ConnEvent::Detached { session_id: req.key.session_id });
                }
                return Ok(Start::Attached { epoch, status, audio_ms });
            }
            Some(Existing::Recording) => {
                // The old audio is discarded and the session starts over with an upload.
                let entry = sessions.remove(&req.key).expect("entry exists");
                if let Some(old) = entry.attached
                    && old.conn_id != req.conn.conn_id
                {
                    old.send(ConnEvent::Detached { session_id: req.key.session_id });
                }
                if let Err(rejection) = (req.admit)() {
                    return Ok(Start::Rejected(rejection));
                }
                let history_id = db.start_session(&req.key.device, req.key.session_id, req.profile)?;
                return Ok(Start::Record { epoch: self.insert_recording(&mut sessions, req), history_id });
            }
            None => {}
        }

        if let Some(stored) = db.session(&req.key.device, req.key.session_id)? {
            if !req.resume {
                return Ok(Start::Rejected(reused()));
            }
            if let Some((fin, timings)) = stored.finished {
                let msg = FinalMsg {
                    session_id: req.key.session_id,
                    history_id: stored.id,
                    r#final: fin,
                    timings,
                    trace: None,
                };
                return Ok(Start::Finished(Arc::new(msg)));
            }
        }

        if let Err(rejection) = (req.admit)() {
            return Ok(Start::Rejected(rejection));
        }
        if count_active(&sessions) >= self.max_active {
            let message = format!("the server is busy with {} sessions", self.max_active);
            return Ok(Start::Rejected(Rejection::new(ErrorCode::Busy, message)));
        }
        let history_id = db.start_session(&req.key.device, req.key.session_id, req.profile)?;
        Ok(Start::Record { epoch: self.insert_recording(&mut sessions, req), history_id })
    }

    fn insert_recording(&self, sessions: &mut HashMap<SessionKey, Entry>, req: StartRequest<'_>) -> u64 {
        let epoch = self.epoch();
        sessions.insert(req.key, Entry { epoch, state: State::Recording, attached: Some(req.conn) });
        epoch
    }

    pub fn stop(
        &self,
        key: &SessionKey,
        epoch: u64,
        conn_id: u64,
        audio_ms: u32,
        submit: impl FnOnce() -> JobHandle,
    ) -> Option<JobHandle> {
        let mut sessions = self.lock();
        let entry = sessions.get_mut(key).filter(|entry| {
            entry.epoch == epoch
                && matches!(entry.state, State::Recording)
                && entry.attached.as_ref().is_some_and(|a| a.conn_id == conn_id)
        })?;
        let handle = submit();
        entry.state = State::Processing { job_id: handle.id, status: handle.status.clone(), audio_ms };
        Some(handle)
    }

    pub fn finish(&self, key: &SessionKey, epoch: u64, msg: FinalMsg) -> bool {
        let mut sessions = self.lock();
        let Some(entry) = sessions
            .get_mut(key)
            .filter(|entry| entry.epoch == epoch && matches!(entry.state, State::Processing { .. }))
        else {
            return false;
        };
        let msg = Arc::new(msg);
        entry.state = State::Finished { msg: msg.clone(), expires: Instant::now() + FINISHED_TTL };
        if let Some(conn) = &entry.attached {
            conn.send(ConnEvent::Msg(ServerMsg::Final(Box::new((*msg).clone()))));
        }
        true
    }

    pub fn fail(&self, key: &SessionKey, epoch: u64, error: ErrorMsg) {
        let mut sessions = self.lock();
        if sessions
            .get(key)
            .is_some_and(|entry| entry.epoch == epoch && matches!(entry.state, State::Processing { .. }))
        {
            let entry = sessions.remove(key).expect("entry exists");
            if let Some(conn) = entry.attached {
                conn.send(ConnEvent::Msg(ServerMsg::Error(error)));
            }
        }
    }

    pub fn drop_recording(&self, key: &SessionKey, epoch: u64) {
        let mut sessions = self.lock();
        if sessions.get(key).is_some_and(|entry| entry.epoch == epoch && matches!(entry.state, State::Recording)) {
            sessions.remove(key);
        }
    }

    pub fn cancel(&self, key: &SessionKey, epoch: u64, conn_id: u64) -> Cancel {
        let mut sessions = self.lock();
        let Some(entry) = sessions.get(key) else { return Cancel::Ignored };
        if entry.epoch != epoch || entry.attached.as_ref().is_none_or(|a| a.conn_id != conn_id) {
            return Cancel::Ignored;
        }
        let job = match &entry.state {
            State::Finished { .. } => return Cancel::Ignored,
            State::Recording => None,
            State::Processing { job_id, .. } => Some(*job_id),
        };
        sessions.remove(key);
        Cancel::Cancelled(job)
    }

    pub fn detach(self: &Arc<Self>, key: &SessionKey, epoch: u64, conn_id: u64, last_frame: Instant) {
        let mut sessions = self.lock();
        let Some(entry) = sessions.get_mut(key).filter(|entry| entry.epoch == epoch) else { return };
        if entry.attached.as_ref().is_none_or(|a| a.conn_id != conn_id) {
            return;
        }
        entry.attached = None;
        if matches!(entry.state, State::Recording) {
            let registry = self.clone();
            let key = key.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(last_frame + ORPHAN_RECORDING_TTL).await;
                let mut sessions = registry.lock();
                if sessions.get(&key).is_some_and(|entry| {
                    entry.epoch == epoch && matches!(entry.state, State::Recording) && entry.attached.is_none()
                }) {
                    tracing::info!("dropping session {} of {}: no audio for 10 s", key.session_id, key.device);
                    sessions.remove(&key);
                }
            });
        }
    }

    pub fn sweep(&self) {
        let now = Instant::now();
        self.lock().retain(|_, entry| !matches!(&entry.state, State::Finished { expires, .. } if *expires <= now));
    }
}

fn count_active(sessions: &HashMap<SessionKey, Entry>) -> usize {
    sessions.values().filter(|entry| !matches!(entry.state, State::Finished { .. })).count()
}
