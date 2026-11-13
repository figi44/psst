use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use jiff::Timestamp;
use psst_proto::SessionId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

pub const STORAGE_BUDGET: u64 = 2_000_000_000;
pub const RETAINED_WARNING: usize = 20;
pub const RETAINED_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub profile: String,
    pub started_at: Timestamp,
    pub samples: u64,
    pub gapped: bool,
    pub state: RecState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecState {
    Recording,
    Complete,
    Incomplete,
    /// Not yet delivered: delivered recordings are deleted.
    Transcribed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Last {
    pub session_id: SessionId,
    pub finalized_at: Timestamp,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Retained {
    pub id: SessionId,
    pub meta: Meta,
    pub wav_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Recovery {
    pub interrupted: Vec<SessionId>,
    pub adopted: Vec<SessionId>,
    pub last_from: Option<SessionId>,
}

#[derive(Debug, Clone)]
pub struct Store {
    state_dir: PathBuf,
    recordings: PathBuf,
    revision: Arc<AtomicU64>,
}

impl Store {
    pub fn open(state_dir: &Path) -> io::Result<Store> {
        let recordings = state_dir.join("recordings");
        fs::create_dir_all(&recordings)?;
        for dir in [state_dir, &recordings] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Store { state_dir: state_dir.into(), recordings, revision: Arc::default() })
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn recordings_dir(&self) -> &Path {
        &self.recordings
    }

    pub fn wav_path(&self, id: SessionId) -> PathBuf {
        self.recordings.join(format!("{id}.wav"))
    }

    pub fn meta_path(&self, id: SessionId) -> PathBuf {
        self.recordings.join(format!("{id}.json"))
    }

    pub fn last_path(&self) -> PathBuf {
        self.state_dir.join("last.json")
    }

    pub fn write_meta(&self, id: SessionId, meta: &Meta) -> io::Result<()> {
        self.changed();
        write_json_atomic(&self.meta_path(id), meta, 0o600)
    }

    pub fn read_meta(&self, id: SessionId) -> io::Result<Meta> {
        read_json(&self.meta_path(id))
    }

    /// The WAV first, so a crash in between can't leave a WAV that `recover` would adopt.
    pub fn delete(&self, id: SessionId) -> io::Result<()> {
        self.changed();
        for path in [self.wav_path(id), self.meta_path(id)] {
            match fs::remove_file(&path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn write_last(&self, last: &Last) -> io::Result<()> {
        write_json_atomic(&self.last_path(), last, 0o600)
    }

    pub fn read_last(&self) -> Option<Last> {
        match read_json(&self.last_path()) {
            Ok(last) => Some(last),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => {
                warn!("ignoring {}: {err}", self.last_path().display());
                None
            }
        }
    }

    pub fn list(&self) -> Vec<Retained> {
        let mut found = Vec::new();
        let Ok(entries) = fs::read_dir(&self.recordings) else { return found };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = session_file(&path, "json") else { continue };
            match read_json::<Meta>(&path) {
                Ok(meta) => {
                    let wav_bytes = fs::metadata(self.wav_path(id)).map_or(0, |m| m.len());
                    found.push(Retained { id, meta, wav_bytes });
                }
                Err(err) => warn!("ignoring {}: {err}", path.display()),
            }
        }
        found.sort_by_key(|r| (r.meta.started_at, r.id));
        found
    }

    pub fn usage(&self) -> u64 {
        fs::read_dir(&self.recordings)
            .map(|entries| entries.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }

    pub fn recover(&self, default_profile: &str) -> io::Result<Recovery> {
        let mut recovery = Recovery::default();
        for dir in [&self.state_dir, &self.recordings] {
            for entry in fs::read_dir(dir)?.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        for entry in fs::read_dir(&self.recordings)?.flatten() {
            let path = entry.path();
            let Some(id) = session_file(&path, "wav") else { continue };
            if self.meta_path(id).exists() {
                continue;
            }
            let started_at = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| Timestamp::try_from(t).ok())
                .unwrap_or_else(Timestamp::now);
            let meta = Meta {
                profile: default_profile.into(),
                started_at,
                samples: wav_samples(&path),
                gapped: false,
                state: RecState::Incomplete,
                final_text: None,
                finalized_at: None,
            };
            self.write_meta(id, &meta)?;
            recovery.adopted.push(id);
        }
        for mut rec in self.list() {
            if rec.meta.state == RecState::Recording {
                rec.meta.state = RecState::Incomplete;
                rec.meta.samples = wav_samples(&self.wav_path(rec.id));
                self.write_meta(rec.id, &rec.meta)?;
                recovery.interrupted.push(rec.id);
            }
        }
        let newest = self
            .list()
            .into_iter()
            .filter(|r| r.meta.state == RecState::Transcribed)
            .filter_map(|r| Some((r.meta.finalized_at?, r.id, r.meta.final_text?)))
            .max_by_key(|(at, _, _)| *at);
        if let Some((finalized_at, session_id, text)) = newest
            && self.read_last().is_none_or(|last| last.finalized_at < finalized_at)
        {
            self.write_last(&Last { session_id, finalized_at, text })?;
            recovery.last_from = Some(session_id);
        }
        if !recovery.interrupted.is_empty() || !recovery.adopted.is_empty() {
            sync_dir(&self.recordings);
        }
        Ok(recovery)
    }

    /// Undelivered recordings are never deleted automatically, only warned about.
    pub fn warnings(&self, retained: &[Retained]) -> Vec<String> {
        let mut warnings = Vec::new();
        if retained.len() > RETAINED_WARNING {
            warnings.push(format!("{} saved recordings: see psst status", retained.len()));
        }
        let cutoff = Timestamp::now() - jiff::SignedDuration::try_from(RETAINED_MAX_AGE).expect("fits");
        let old = retained.iter().filter(|r| r.meta.started_at < cutoff).count();
        if old > 0 {
            warnings.push(format!("{old} saved recording(s) older than 7 days: psst discard or retry"));
        }
        warnings
    }
}

fn session_file(path: &Path, ext: &str) -> Option<SessionId> {
    if path.extension()? != ext {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

/// From the file size, since a crash can leave the header unfinalised.
pub fn wav_samples(path: &Path) -> u64 {
    psst_client::read_wav_info(path).map_or(0, |info| info.samples)
}

/// No fsync: a rename already survives a crash of the daemon.
pub fn write_json_atomic(path: &Path, value: &impl Serialize, mode: u32) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
    serde_json::to_writer_pretty(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    drop(file);
    fs::rename(&tmp, path)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Blocks on fsync: keep it off the critical path.
pub fn sync_files(files: &[PathBuf], dir: &Path) {
    for path in files {
        if let Err(err) = File::open(path).and_then(|f| f.sync_all())
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!("cannot fsync {}: {err}", path.display());
        }
    }
    sync_dir(dir);
}

pub fn sync_dir(dir: &Path) {
    if let Err(err) = File::open(dir).and_then(|f| f.sync_all()) {
        warn!("cannot fsync {}: {err}", dir.display());
    }
}

#[cfg(test)]
mod tests {
    use psst_testkit::{test_audio, write_wav};

    use super::*;

    fn meta(state: RecState, started_at: Timestamp) -> Meta {
        Meta {
            profile: "default".into(),
            started_at,
            samples: 0,
            gapped: false,
            state,
            final_text: None,
            finalized_at: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("psst")).unwrap();
        (dir, store)
    }

    #[test]
    fn recovery_marks_interrupted_recordings_incomplete_with_their_length() {
        let (_dir, store) = store();
        let id = SessionId::new_v4();
        store.write_meta(id, &meta(RecState::Recording, Timestamp::now())).unwrap();
        write_wav(&store.wav_path(id), &test_audio(1_500));

        let recovery = store.recover("default").unwrap();
        assert_eq!(recovery.interrupted, vec![id]);
        let meta = store.read_meta(id).unwrap();
        assert_eq!(meta.state, RecState::Incomplete);
        assert_eq!(meta.samples, 24_000);
    }

    #[test]
    fn recovery_adopts_a_wav_without_metadata_and_never_deletes_it() {
        let (_dir, store) = store();
        let id = SessionId::new_v4();
        write_wav(&store.wav_path(id), &test_audio(100));
        fs::write(store.recordings_dir().join("x.json.tmp"), "{").unwrap();

        let recovery = store.recover("czech").unwrap();
        assert_eq!(recovery.adopted, vec![id]);
        let meta = store.read_meta(id).unwrap();
        assert_eq!((meta.state, meta.samples, meta.profile.as_str()), (RecState::Incomplete, 1_600, "czech"));
        assert!(store.wav_path(id).exists());
        assert!(!store.recordings_dir().join("x.json.tmp").exists());
    }

    #[test]
    fn recovery_updates_last_json_from_a_newer_transcribed_recording_only() {
        let (_dir, store) = store();
        let old = Timestamp::from_second(1_800_000_000).unwrap();
        let new = Timestamp::from_second(1_800_000_100).unwrap();
        let id = SessionId::new_v4();
        let mut transcribed = meta(RecState::Transcribed, old);
        transcribed.final_text = Some("newer text".into());
        transcribed.finalized_at = Some(new);
        store.write_meta(id, &transcribed).unwrap();
        store.write_last(&Last { session_id: SessionId::new_v4(), finalized_at: old, text: "older".into() }).unwrap();

        assert_eq!(store.recover("default").unwrap().last_from, Some(id));
        let last = store.read_last().unwrap();
        assert_eq!((last.session_id, last.text.as_str()), (id, "newer text"));
        assert_eq!(fs::metadata(store.last_path()).unwrap().permissions().mode() & 0o777, 0o600);

        let later = Last {
            session_id: SessionId::new_v4(),
            finalized_at: new + jiff::SignedDuration::from_secs(5),
            text: "latest".into(),
        };
        store.write_last(&later).unwrap();
        assert_eq!(store.recover("default").unwrap().last_from, None);
        assert_eq!(store.read_last().unwrap(), later);
    }

    #[test]
    fn recovery_keeps_every_undelivered_recording_however_old() {
        let (_dir, store) = store();
        let ancient = Timestamp::from_second(1_000_000_000).unwrap();
        let ids: Vec<_> = (0..25).map(|_| SessionId::new_v4()).collect();
        for id in &ids {
            store.write_meta(*id, &meta(RecState::Complete, ancient)).unwrap();
            write_wav(&store.wav_path(*id), &[0; 16]);
        }
        store.recover("default").unwrap();
        let retained = store.list();
        assert_eq!(retained.len(), 25);
        let warnings = store.warnings(&retained);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn delete_removes_both_files_and_tolerates_missing_ones() {
        let (_dir, store) = store();
        let id = SessionId::new_v4();
        store.write_meta(id, &meta(RecState::Complete, Timestamp::now())).unwrap();
        write_wav(&store.wav_path(id), &[1, 2, 3]);
        assert!(store.usage() > 0);
        store.delete(id).unwrap();
        store.delete(id).unwrap();
        assert!(store.list().is_empty());
        assert_eq!(store.usage(), 0);
    }
}
