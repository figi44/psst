//! The server's SQLite database (`<state dir>/psst.db`, WAL): devices, history and small pieces of
//! state such as a deliberate unload.
//!
//! Every statement is short, so callers run them inline under one connection mutex.

use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{Context, Result};
use jiff::Timestamp;
use psst_proto::{
    ClientTimings, Final, HistoryId, ServerTimings, SessionId,
    api::{Device, DeviceKind, HistoryRow},
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
}
