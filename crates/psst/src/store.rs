//! Recordings, `last.json` and credentials on disk (plan 9.3), under `$XDG_STATE_HOME/psst/`.
//!
//! - `recordings/<session_id>.wav`: 16 kHz mono s16, written as audio is captured. Its length
//!   comes from the file size, so a header that was never finalised doesn't matter.
//! - `recordings/<session_id>.json`: [`Meta`], always written atomically (a temporary file, then
//!   a rename).
//! - `last.json`: [`Last`], mode 0600, atomic.
//!
//! A recording is created JSON first, then WAV, and deleted WAV first, then JSON. Undelivered
//! recordings are never deleted automatically: only delivery, an empty final, `cancel` and
//! `psst discard` remove them.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use jiff::Timestamp;
use psst_proto::SessionId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

/// `start` refuses to record while the recordings take more than this.
pub const STORAGE_BUDGET: u64 = 2_000_000_000;
/// More retained recordings than this is a warning.
pub const RETAINED_WARNING: usize = 20;
/// A retained recording older than this is a warning.
pub const RETAINED_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// `<session_id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub profile: String,
    pub started_at: Timestamp,
    /// Real (unpadded) samples in the WAV.
    pub samples: u64,
    /// Audio glitches were filled with silence.
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
    /// Being captured (or the daemon died while capturing).
    Recording,
    /// Captured in full and not yet transcribed.
    Complete,
    /// Capture stopped early (a write failure or a lost device), or the daemon died.
    Incomplete,
    /// Transcribed; the text hasn't been delivered.
    Transcribed,
}

/// `last.json`: the newest delivered-or-deliverable dictation, for `paste-last`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Last {
    pub session_id: SessionId,
    pub finalized_at: Timestamp,
    pub text: String,
}

/// A recording on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Retained {
    pub id: SessionId,
    pub meta: Meta,
    /// Bytes of the WAV, 0 if it's gone.
    pub wav_bytes: u64,
}

/// What [`Store::recover`] changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Recovery {
    /// Recordings that were still `recording` and are now `incomplete`.
    pub interrupted: Vec<SessionId>,
    /// WAV files without metadata that were adopted as `incomplete`.
    pub adopted: Vec<SessionId>,
    /// `last.json` was updated from this newer transcribed recording.
    pub last_from: Option<SessionId>,
}

#[derive(Debug, Clone)]
pub struct Store {
    state_dir: PathBuf,
    recordings: PathBuf,
}

impl Store {
    /// Opens (and creates) `state_dir` and its `recordings/`, both mode 0700.
    pub fn open(state_dir: &Path) -> io::Result<Store> {
        let recordings = state_dir.join("recordings");
        fs::create_dir_all(&recordings)?;
        for dir in [state_dir, &recordings] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Store { state_dir: state_dir.into(), recordings })
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
        write_json_atomic(&self.meta_path(id), meta, 0o600)
    }

    pub fn read_meta(&self, id: SessionId) -> io::Result<Meta> {
        read_json(&self.meta_path(id))
    }

    /// Deletes a recording: the WAV first, then its metadata.
    pub fn delete(&self, id: SessionId) -> io::Result<()> {
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

    /// Every recording with readable metadata, oldest first.
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

    /// Bytes used by the recordings directory.
    pub fn usage(&self) -> u64 {
        fs::read_dir(&self.recordings)
            .map(|entries| entries.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }

    /// Repairs what a crash may have left, at daemon start (plan 9.3). Nothing is sent or
    /// pasted, and nothing undelivered is deleted.
    ///
    /// - Recordings still `recording` become `incomplete`, with `samples` from the WAV.
    /// - A WAV without metadata is adopted as `incomplete` with `default_profile`.
    /// - If a `transcribed` recording is newer than `last.json`, `last.json` is updated from it.
    /// - Leftover temporary files are removed.
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

    /// Warnings about retained recordings: too many, or too old.
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

/// The session id of `<uuid>.<ext>`.
fn session_file(path: &Path, ext: &str) -> Option<SessionId> {
    if path.extension()? != ext {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok()
}

/// Samples in a WAV, from its size; 0 if it's missing or unreadable.
pub fn wav_samples(path: &Path) -> u64 {
    psst_client::read_wav_info(path).map_or(0, |info| info.samples)
}

/// Writes `value` as JSON to a temporary file next to `path` with `mode`, then renames it over
/// `path`. No fsync: a rename already survives a crash of the daemon.
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

/// fsyncs files and then their directory, logging failures. Called off the critical path.
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
        Meta { profile: "default".into(), started_at, samples: 0, gapped: false, state, final_text: None, finalized_at: None }
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

        // last.json is already newer than every recording: left alone.
        let later = Last { session_id: SessionId::new_v4(), finalized_at: new + jiff::SignedDuration::from_secs(5), text: "latest".into() };
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
