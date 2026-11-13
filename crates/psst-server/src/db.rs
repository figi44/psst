//! The server's SQLite database (`<state dir>/psst.db`, WAL): devices, history, the dictionary's
//! database layer, profiles stored over `profiles.toml`, and small pieces of state such as a
//! deliberate unload.
//!
//! Every statement is short, so callers run them inline under one connection mutex.

use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{Context, Result};
use jiff::Timestamp;
use psst_config::TermSpec;
use psst_dict::{LayerEntry, LayerTerm};
use psst_proto::{
    ClientTimings, Final, HistoryId, ServerTimings, SessionId, TermId,
    api::{Device, DeviceKind, HistoryRow, NeverRuleId, Profile, Term},
};
use rand::RngExt;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('local', 'token')),
    token_hash TEXT UNIQUE,
    created_ms INTEGER NOT NULL,
    last_seen_ms INTEGER,
    revoked_ms INTEGER
);
CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    device TEXT NOT NULL,
    session_id TEXT NOT NULL,
    profile TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    model TEXT,
    revision TEXT,
    final TEXT,
    server_timings TEXT,
    client_timings TEXT,
    UNIQUE (device, session_id)
);
CREATE INDEX IF NOT EXISTS history_created ON history (created_ms);
CREATE TABLE IF NOT EXISTS state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- The dictionary's database layer (plan 7.1): one entry per term identity.
CREATE TABLE IF NOT EXISTS dictionary_terms (
    id TEXT PRIMARY KEY,
    -- The term (an API `Term` as JSON), which adds it or replaces the file's; NULL hides the file's.
    term TEXT,
    -- Order of saving: of two conflicting entries, the later one gives way.
    saved INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dictionary_never (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    term TEXT NOT NULL,
    span TEXT NOT NULL,
    span_key TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    UNIQUE (term, span_key)
);
-- Whole profiles by name, each replacing profiles.toml's profile with that name or adding one
-- (plan 6.5).
CREATE TABLE IF NOT EXISTS profiles (
    name TEXT PRIMARY KEY,
    -- An API `Profile` as JSON.
    profile TEXT NOT NULL,
    updated_ms INTEGER NOT NULL
);
";

/// Key in `state` holding the boot id of a deliberate unload.
pub const UNLOADED_BOOT_ID: &str = "unloaded_boot_id";

#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

/// A paired device found by its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenDevice {
    pub id: String,
    pub name: String,
}

/// A session's history row, as `start` needs it.
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: HistoryId,
    /// Present once the session finished.
    pub finished: Option<(Final, ServerTimings)>,
}

/// A never rule as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNever {
    pub id: NeverRuleId,
    pub term: TermId,
    pub span: String,
    pub span_key: String,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoke {
    Revoked,
    NotFound,
    /// Local devices are allowed by `local_users`, not by a token.
    Local,
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("stored types serialise")
}

fn parse<T: serde::de::DeserializeOwned>(text: Option<String>) -> Option<T> {
    text.and_then(|text| serde_json::from_str(&text).ok())
}

impl Db {
    /// Opens (or creates) the database at `path`.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!("{}: journal mode is {mode}, not WAL", path.display());
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(SCHEMA).with_context(|| format!("cannot set up {}", path.display()))?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records a local user's device, `local:<user>`, as seen now.
    pub fn touch_local(&self, user: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO devices (id, name, kind, created_ms, last_seen_ms) VALUES (?1, ?2, 'local', ?3, ?3)
             ON CONFLICT (id) DO UPDATE SET last_seen_ms = excluded.last_seen_ms, revoked_ms = NULL",
            params![format!("local:{user}"), user, now_ms()],
        )?;
        Ok(())
    }

    /// Adds a paired device and returns its new id.
    pub fn add_token_device(&self, name: &str, token_hash: &str) -> Result<String> {
        let conn = self.conn();
        loop {
            let id = format!("{:08x}", rand::rng().random::<u32>());
            let inserted = conn.execute(
                "INSERT INTO devices (id, name, kind, token_hash, created_ms) VALUES (?1, ?2, 'token', ?3, ?4)",
                params![id, name, token_hash, now_ms()],
            );
            match inserted {
                Ok(_) => return Ok(id),
                Err(rusqlite::Error::SqliteFailure(err, _)) if err.code == ErrorCode::ConstraintViolation => {
                    // An id collision; a token collision can't happen with 238 random bits.
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// The device holding this token, unless it was revoked.
    pub fn device_by_token(&self, token_hash: &str) -> Result<Option<TokenDevice>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, name FROM devices WHERE token_hash = ?1 AND revoked_ms IS NULL",
                params![token_hash],
                |row| Ok(TokenDevice { id: row.get(0)?, name: row.get(1)? }),
            )
            .optional()?)
    }

    /// Whether the device exists and isn't revoked.
    pub fn device_active(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row("SELECT 1 FROM devices WHERE id = ?1 AND revoked_ms IS NULL", params![id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        self.conn().execute("UPDATE devices SET last_seen_ms = ?2 WHERE id = ?1", params![id, now_ms()])?;
        Ok(())
    }

    /// Devices that aren't revoked, oldest first.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, created_ms, last_seen_ms FROM devices WHERE revoked_ms IS NULL
             ORDER BY created_ms, id",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind: String = row.get(2)?;
            Ok(Device {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: if kind == "local" { DeviceKind::Local } else { DeviceKind::Token },
                created_at: timestamp(row.get(3)?),
                last_seen: row.get::<_, Option<i64>>(4)?.map(timestamp),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke(&self, id: &str) -> Result<Revoke> {
        let conn = self.conn();
        let kind: Option<String> = conn
            .query_row("SELECT kind FROM devices WHERE id = ?1 AND revoked_ms IS NULL", params![id], |row| row.get(0))
            .optional()?;
        match kind.as_deref() {
            None => Ok(Revoke::NotFound),
            Some("local") => Ok(Revoke::Local),
            Some(_) => {
                conn.execute("UPDATE devices SET revoked_ms = ?2 WHERE id = ?1", params![id, now_ms()])?;
                Ok(Revoke::Revoked)
            }
        }
    }

    /// The history row of (device, session), if there is one.
    pub fn session(&self, device: &str, session_id: SessionId) -> Result<Option<StoredSession>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, final, server_timings FROM history WHERE device = ?1 AND session_id = ?2",
                params![device, session_id.to_string()],
                |row| {
                    let id: i64 = row.get(0)?;
                    let fin: Option<Final> = parse(row.get(1)?);
                    let timings: Option<ServerTimings> = parse(row.get(2)?);
                    Ok(StoredSession { id: id as HistoryId, finished: fin.zip(timings) })
                },
            )
            .optional()?)
    }

    /// Creates the history row of (device, session), or returns the existing one's id.
    pub fn start_session(&self, device: &str, session_id: SessionId, profile: &str) -> Result<HistoryId> {
        let id: i64 = self.conn().query_row(
            "INSERT INTO history (device, session_id, profile, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (device, session_id) DO UPDATE SET profile = excluded.profile
             RETURNING id",
            params![device, session_id.to_string(), profile, now_ms()],
            |row| row.get(0),
        )?;
        Ok(id as HistoryId)
    }

    pub fn finish_session(
        &self,
        id: HistoryId,
        model: &str,
        revision: &str,
        fin: &Final,
        timings: &ServerTimings,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE history SET model = ?2, revision = ?3, final = ?4, server_timings = ?5 WHERE id = ?1",
            params![id as i64, model, revision, json(fin), json(timings)],
        )?;
        Ok(())
    }

    /// Stores a client's timings on its own session's row; anything else is ignored.
    pub fn report(&self, device: &str, session_id: SessionId, id: HistoryId, timings: &ClientTimings) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE history SET client_timings = ?4 WHERE id = ?1 AND device = ?2 AND session_id = ?3",
            params![id as i64, device, session_id.to_string(), json(timings)],
        )?;
        Ok(updated > 0)
    }

    /// The newest `limit` rows, newest first.
    pub fn history(&self, limit: u32) -> Result<Vec<HistoryRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, device, profile, created_ms, model, revision, final, server_timings,
                    client_timings
             FROM history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            let session_id: String = row.get(1)?;
            Ok(HistoryRow {
                id: row.get::<_, i64>(0)? as HistoryId,
                session_id: session_id.parse().unwrap_or_default(),
                device: row.get(2)?,
                profile: row.get(3)?,
                created_at: timestamp(row.get(4)?),
                model: row.get(5)?,
                revision: row.get(6)?,
                r#final: parse(row.get(7)?),
                server_timings: parse(row.get(8)?),
                client_timings: parse(row.get(9)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Timings of every finished session.
    pub fn timings(&self) -> Result<Vec<(ServerTimings, Option<ClientTimings>)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT server_timings, client_timings FROM history WHERE server_timings IS NOT NULL")?;
        let rows = stmt.query_map([], |row| Ok((parse(row.get(0)?), parse(row.get(1)?))))?;
        let mut out = Vec::new();
        for row in rows {
            if let (Some(server), client) = row? {
                out.push((server, client));
            }
        }
        Ok(out)
    }

    /// Deletes history rows created more than `days` days ago.
    pub fn prune_history(&self, days: u32) -> Result<usize> {
        let cutoff = now_ms() - i64::from(days) * 86_400_000;
        Ok(self.conn().execute("DELETE FROM history WHERE created_ms < ?1", params![cutoff])?)
    }

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM state WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO state (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn clear_state(&self, key: &str) -> Result<()> {
        self.conn().execute("DELETE FROM state WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// The dictionary's database layer, by identity. An entry that can't be read is logged and
    /// left out.
    pub fn dictionary_terms(&self) -> Result<std::collections::BTreeMap<TermId, LayerTerm>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, term, saved FROM dictionary_terms")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?))
        })?;
        let mut terms = std::collections::BTreeMap::new();
        for row in rows {
            let (id, term, saved) = row?;
            let entry = match term {
                None => LayerEntry::Hide,
                Some(json) => match serde_json::from_str::<Term>(&json) {
                    Ok(term) => LayerEntry::Set(TermSpec::from(term)),
                    Err(err) => {
                        tracing::warn!("ignoring the dictionary entry for `{id}`, which can't be read: {err}");
                        continue;
                    }
                },
            };
            terms.insert(id, LayerTerm { entry, saved: saved as u64 });
        }
        Ok(terms)
    }

    /// Stores the entry for an identity, replacing any it had.
    pub fn put_dictionary_term(&self, id: &str, entry: &LayerEntry, saved: u64) -> Result<()> {
        let term = match entry {
            LayerEntry::Set(spec) => Some(json(&Term::from(spec))),
            LayerEntry::Hide => None,
        };
        self.conn().execute(
            "INSERT INTO dictionary_terms (id, term, saved, updated_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET term = excluded.term, saved = excluded.saved,
                                            updated_ms = excluded.updated_ms",
            params![id, term, saved as i64, now_ms()],
        )?;
        Ok(())
    }

    /// Deletes the entry for an identity; false if it had none.
    pub fn delete_dictionary_term(&self, id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM dictionary_terms WHERE id = ?1", params![id])? > 0)
    }

    /// Every never rule, oldest first.
    pub fn never_rules(&self) -> Result<Vec<StoredNever>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, term, span, span_key, created_ms FROM dictionary_never ORDER BY id")?;
        let rows = stmt.query_map([], never_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a never rule, or returns the one with the same term and span key.
    pub fn add_never_rule(&self, term: &str, span: &str, span_key: &str) -> Result<(StoredNever, bool)> {
        let conn = self.conn();
        let added = conn.execute(
            "INSERT INTO dictionary_never (term, span, span_key, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (term, span_key) DO NOTHING",
            params![term, span, span_key, now_ms()],
        )? > 0;
        let rule = conn.query_row(
            "SELECT id, term, span, span_key, created_ms FROM dictionary_never WHERE term = ?1 AND span_key = ?2",
            params![term, span_key],
            never_row,
        )?;
        Ok((rule, added))
    }

    /// Deletes a never rule; false if there's none with this id.
    pub fn delete_never_rule(&self, id: NeverRuleId) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM dictionary_never WHERE id = ?1", params![id as i64])? > 0)
    }
}

impl Db {
    /// The stored profiles by name; one that can't be read comes with the reason.
    pub fn profiles(&self) -> Result<Vec<(String, std::result::Result<Profile, String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, profile FROM profiles ORDER BY name")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut profiles = Vec::new();
        for row in rows {
            let (name, profile) = row?;
            profiles.push((name, serde_json::from_str(&profile).map_err(|err| err.to_string())));
        }
        Ok(profiles)
    }

    /// Stores a profile, replacing the one stored under its name.
    pub fn put_profile(&self, name: &str, profile: &Profile) -> Result<()> {
        self.conn().execute(
            "INSERT INTO profiles (name, profile, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET profile = excluded.profile, updated_ms = excluded.updated_ms",
            params![name, json(profile), now_ms()],
        )?;
        Ok(())
    }

    /// Deletes a stored profile; false if there's none by this name.
    pub fn delete_profile(&self, name: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM profiles WHERE name = ?1", params![name])? > 0)
    }
}

fn never_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNever> {
    Ok(StoredNever {
        id: row.get::<_, i64>(0)? as NeverRuleId,
        term: row.get(1)?,
        span: row.get(2)?,
        span_key: row.get(3)?,
        created_at: timestamp(row.get(4)?),
    })
}
