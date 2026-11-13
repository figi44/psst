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
    ClientTimings, DeviceId, Final, HistoryId, ServerTimings, SessionId, TermId,
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
CREATE TABLE IF NOT EXISTS profiles (
    name TEXT PRIMARY KEY,
    profile TEXT NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS web_sessions (
    token_hash TEXT PRIMARY KEY,
    -- Who asked for the login link: `local:<user>`, or a paired device's id.
    device TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS web_sessions_device ON web_sessions (device);
-- Devices that paired themselves through `auto_pair`.
CREATE TABLE IF NOT EXISTS tailnet_devices (
    device TEXT PRIMARY KEY,
    -- Tailscale's StableID for the machine.
    node TEXT NOT NULL
);
";

pub const UNLOADED_BOOT_ID: &str = "unloaded_boot_id";

#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: HistoryId,
    pub finished: Option<(Final, ServerTimings)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNever {
    pub id: NeverRuleId,
    pub term: TermId,
    pub span: String,
    pub span_key: String,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailnetPairing {
    Paired {
        id: String,
        replaced: Option<String>,
    },
    /// The machine's device, which has connected.
    Taken(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoke {
    Revoked,
    NotFound,
    Local,
}

fn revoke_device(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("UPDATE devices SET revoked_ms = ?2 WHERE id = ?1", params![id, now_ms()])?;
    conn.execute("DELETE FROM web_sessions WHERE device = ?1", params![id])?;
    Ok(())
}

fn insert_token_device(conn: &Connection, name: &str, token_hash: &str) -> Result<String> {
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

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

const HISTORY_COLUMNS: &str =
    "SELECT id, session_id, device, profile, created_ms, model, revision, final, server_timings, client_timings";

/// Reads columns by index: keep it in step with [`HISTORY_COLUMNS`].
fn history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRow> {
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

    pub fn touch_local(&self, user: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO devices (id, name, kind, created_ms, last_seen_ms) VALUES (?1, ?2, 'local', ?3, ?3)
             ON CONFLICT (id) DO UPDATE SET last_seen_ms = excluded.last_seen_ms, revoked_ms = NULL",
            params![format!("local:{user}"), user, now_ms()],
        )?;
        Ok(())
    }

    pub fn add_token_device(&self, name: &str, token_hash: &str) -> Result<String> {
        insert_token_device(&self.conn(), name, token_hash)
    }

    /// One live device per machine, so nothing else on it (a container, another user) can pair
    /// beside its daemon. A device that never connected gives way: its token was lost on the way
    /// back, which a lossy link or a client's time budget can do.
    pub fn add_tailnet_device(&self, name: &str, node: &str, token_hash: &str) -> Result<TailnetPairing> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let live: Option<(String, Option<i64>)> = tx
            .query_row(
                "SELECT d.id, d.last_seen_ms FROM tailnet_devices t JOIN devices d ON d.id = t.device
                 WHERE t.node = ?1 AND d.revoked_ms IS NULL",
                params![node],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let replaced = match live {
            Some((id, Some(_))) => return Ok(TailnetPairing::Taken(id)),
            Some((id, None)) => {
                revoke_device(&tx, &id)?;
                Some(id)
            }
            None => None,
        };
        let id = insert_token_device(&tx, name, token_hash)?;
        tx.execute("INSERT INTO tailnet_devices (device, node) VALUES (?1, ?2)", params![id, node])?;
        tx.commit()?;
        Ok(TailnetPairing::Paired { id, replaced })
    }

    /// Revokes the devices of machines that `auto_pair` no longer lists, as `(id, name)`.
    pub fn revoke_tailnet_devices_except(&self, names: &[String]) -> Result<Vec<(String, String)>> {
        let live: Vec<(String, String)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT d.id, d.name FROM tailnet_devices t JOIN devices d ON d.id = t.device
                 WHERE d.revoked_ms IS NULL",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let dropped: Vec<_> = live.into_iter().filter(|(_, name)| !names.contains(name)).collect();
        for (id, _) in &dropped {
            self.revoke(id)?;
        }
        Ok(dropped)
    }

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
                revoke_device(&conn, id)?;
                Ok(Revoke::Revoked)
            }
        }
    }

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

    pub fn report(&self, device: &str, session_id: SessionId, id: HistoryId, timings: &ClientTimings) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE history SET client_timings = ?4 WHERE id = ?1 AND device = ?2 AND session_id = ?3",
            params![id as i64, device, session_id.to_string(), json(timings)],
        )?;
        Ok(updated > 0)
    }

    pub fn history(&self, limit: u32, device: Option<&str>) -> Result<Vec<HistoryRow>> {
        let conn = self.conn();
        let filter = if device.is_some() { "WHERE device = ?2 " } else { "" };
        let mut stmt = conn.prepare(&format!("{HISTORY_COLUMNS} FROM history {filter}ORDER BY id DESC LIMIT ?1"))?;
        let rows = match device {
            Some(device) => stmt.query_map(params![limit, device], history_row)?,
            None => stmt.query_map(params![limit], history_row)?,
        };
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn history_row(&self, id: HistoryId) -> Result<Option<HistoryRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("{HISTORY_COLUMNS} FROM history WHERE id = ?1"))?;
        Ok(stmt.query_row(params![id as i64], history_row).optional()?)
    }

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

    pub fn store_dictionary_terms(&self, terms: &[(TermId, Option<LayerTerm>)]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (id, term) in terms {
            match term {
                Some(LayerTerm { entry, saved }) => {
                    let term = match entry {
                        LayerEntry::Set(spec) => Some(json(&Term::from(spec))),
                        LayerEntry::Hide => None,
                    };
                    tx.execute(
                        "INSERT INTO dictionary_terms (id, term, saved, updated_ms) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (id) DO UPDATE SET term = excluded.term, saved = excluded.saved,
                                                        updated_ms = excluded.updated_ms",
                        params![id, term, *saved as i64, now_ms()],
                    )?;
                }
                None => {
                    tx.execute("DELETE FROM dictionary_terms WHERE id = ?1", params![id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn never_rules(&self) -> Result<Vec<StoredNever>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, term, span, span_key, created_ms FROM dictionary_never ORDER BY id")?;
        let rows = stmt.query_map([], never_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

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

    pub fn delete_never_rule(&self, id: NeverRuleId) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM dictionary_never WHERE id = ?1", params![id as i64])? > 0)
    }
}

impl Db {
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

    pub fn put_profile(&self, name: &str, profile: &Profile) -> Result<()> {
        self.conn().execute(
            "INSERT INTO profiles (name, profile, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET profile = excluded.profile, updated_ms = excluded.updated_ms",
            params![name, json(profile), now_ms()],
        )?;
        Ok(())
    }

    pub fn add_web_session(&self, token_hash: &str, device: &str, expires: Timestamp) -> Result<()> {
        self.conn().execute(
            "INSERT INTO web_sessions (token_hash, device, created_ms, expires_ms) VALUES (?1, ?2, ?3, ?4)",
            params![token_hash, device, now_ms(), expires.as_millisecond()],
        )?;
        Ok(())
    }

    pub fn web_session(&self, token_hash: &str) -> Result<Option<DeviceId>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT device FROM web_sessions WHERE token_hash = ?1 AND expires_ms > ?2",
                params![token_hash, now_ms()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn delete_web_session(&self, token_hash: &str) -> Result<()> {
        self.conn().execute("DELETE FROM web_sessions WHERE token_hash = ?1", params![token_hash])?;
        Ok(())
    }

    pub fn prune_web_sessions(&self) -> Result<usize> {
        Ok(self.conn().execute("DELETE FROM web_sessions WHERE expires_ms <= ?1", params![now_ms()])?)
    }

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
