//! Centralised SQLite database for all persistent panel state.
//!
//! A single file — `{config_dir}/panel.db` — replaces the previous
//! collection of JSON files and flat log files:
//!
//!  - `users`       : user accounts (was users.json)
//!  - `tls_config`  : TLS / port settings (was tls.json)
//!  - `console_log` : per-server console history (was server.log files)
//!
//! All callers interact through the `Db` handle which is cheaply `Clone`-able
//! (it wraps an `Arc<Mutex<Connection>>`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

// ── Handle ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (or create) the database at `{config_dir}/panel.db` and
    /// run all migrations. Safe to call on every startup.
    pub fn open(config_dir: &str) -> Result<Self> {
        let path = PathBuf::from(config_dir).join("panel.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        // Performance pragmas — safe for our single-writer use case.
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous  = NORMAL;
            PRAGMA foreign_keys = ON;
        ")?;

        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("
            -- ── Users ────────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS users (
                username        TEXT PRIMARY KEY,
                password        TEXT NOT NULL,
                role            TEXT NOT NULL DEFAULT 'admin',
                allowed_servers TEXT          -- JSON array or NULL
            );

            -- ── TLS config ───────────────────────────────────────────────
            -- Single row keyed by id = 1.
            CREATE TABLE IF NOT EXISTS tls_config (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                enabled     INTEGER NOT NULL DEFAULT 0,
                domain      TEXT,
                cert        TEXT,
                key         TEXT,
                http_port   INTEGER NOT NULL DEFAULT 80,
                https_port  INTEGER NOT NULL DEFAULT 443
            );

            -- ── Console log ──────────────────────────────────────────────
            -- `server_id` matches the server's registry ID.
            -- `kind` is a short tag: 'log', 'rcon', 'panel', 'warn', 'error'.
            -- `ts` is Unix milliseconds (INTEGER for compact storage).
            -- Rows are pruned to MAX_LOG_ROWS per server on insert.
            CREATE TABLE IF NOT EXISTS console_log (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id TEXT    NOT NULL,
                ts        INTEGER NOT NULL,
                kind      TEXT    NOT NULL DEFAULT 'log',
                text      TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_console_log_server
                ON console_log (server_id, id);

            -- ── Player activity log ───────────────────────────────────────
            -- Persistent log of player join / leave / death events.
            -- `event` is one of: 'join', 'leave', 'death'.
            -- `player` is the player name.
            -- `detail` holds extra context (e.g. the death message) or NULL.
            -- `ts` is Unix milliseconds.
            CREATE TABLE IF NOT EXISTS player_activity (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id TEXT    NOT NULL,
                ts        INTEGER NOT NULL,
                event     TEXT    NOT NULL,
                player    TEXT    NOT NULL,
                detail    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_player_activity_server
                ON player_activity (server_id, id);
        ")?;
        Ok(())
    }

    // ── User helpers (used by users.rs) ───────────────────────────────────

    pub fn users_all(&self) -> Result<Vec<crate::users::User>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT username, password, role, allowed_servers FROM users ORDER BY rowid"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::users::User {
                username:        row.get(0)?,
                password:        row.get(1)?,
                role:            row.get::<_, String>(2)?.parse().unwrap_or(crate::users::UserRole::Viewer),
                allowed_servers: row.get::<_, Option<String>>(3)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn user_upsert(&self, u: &crate::users::User) -> Result<()> {
        let allowed = u.allowed_servers.as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        self.conn.lock().unwrap().execute(
            "INSERT INTO users (username, password, role, allowed_servers)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(username) DO UPDATE SET
               password        = excluded.password,
               role            = excluded.role,
               allowed_servers = excluded.allowed_servers",
            params![u.username, u.password, u.role.to_string(), allowed],
        )?;
        Ok(())
    }

    pub fn user_delete(&self, username: &str) -> Result<()> {
        self.conn.lock().unwrap()
            .execute("DELETE FROM users WHERE username = ?1", params![username])?;
        Ok(())
    }

    pub fn user_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users", [], |r| r.get(0)
        )?;
        Ok(n as usize)
    }

    pub fn admin_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| r.get(0)
        )?;
        Ok(n as usize)
    }

    // ── TLS helpers (used by setup.rs) ────────────────────────────────────

    pub fn tls_load(&self) -> Result<Option<crate::setup::TlsConfig>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(bool, Option<String>, Option<String>, Option<String>, u16, u16)> =
            conn.query_row(
                "SELECT enabled, domain, cert, key, http_port, https_port
                 FROM tls_config WHERE id = 1",
                [],
                |r| Ok((
                    r.get::<_, bool>(0)?,
                    r.get(1)?, r.get(2)?, r.get(3)?,
                    r.get(4)?, r.get(5)?,
                )),
            ).optional()?;
        Ok(row.map(|(enabled, domain, cert, key, http_port, https_port)| {
            crate::setup::TlsConfig { enabled, domain, cert, key, http_port, https_port }
        }))
    }

    pub fn tls_save(&self, cfg: &crate::setup::TlsConfig) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO tls_config (id, enabled, domain, cert, key, http_port, https_port)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               enabled    = excluded.enabled,
               domain     = excluded.domain,
               cert       = excluded.cert,
               key        = excluded.key,
               http_port  = excluded.http_port,
               https_port = excluded.https_port",
            params![
                cfg.enabled, cfg.domain, cfg.cert, cfg.key,
                cfg.http_port, cfg.https_port
            ],
        )?;
        Ok(())
    }

    // ── Console log helpers (used by logbuffer.rs) ────────────────────────

    /// Maximum rows kept per server. Older rows are pruned on insert.
    pub const MAX_LOG_ROWS: usize = 2000;

    /// Load the last `limit` lines for a server (oldest first).
    pub fn log_load(&self, server_id: &str, limit: usize) -> Result<Vec<crate::logbuffer::LogLine>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, text FROM (
                SELECT id, kind, text FROM console_log
                WHERE server_id = ?1
                ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC"
        )?;
        let rows = stmt.query_map(params![server_id, limit as i64], |r| {
            Ok(crate::logbuffer::LogLine {
                kind: r.get(0)?,
                text: r.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Append a line and prune excess rows in one transaction.
    pub fn log_append(&self, server_id: &str, kind: &str, text: &str) -> Result<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO console_log (server_id, ts, kind, text) VALUES (?1, ?2, ?3, ?4)",
            params![server_id, now_ms, kind, text],
        )?;
        // Prune: delete oldest rows beyond the cap.
        conn.execute(
            "DELETE FROM console_log
             WHERE server_id = ?1
               AND id NOT IN (
                 SELECT id FROM console_log
                 WHERE server_id = ?1
                 ORDER BY id DESC LIMIT ?2
               )",
            params![server_id, Self::MAX_LOG_ROWS as i64],
        )?;
        Ok(())
    }

    /// Delete all stored lines for a server (the "clear" action).
    pub fn log_clear(&self, server_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM console_log WHERE server_id = ?1",
            params![server_id],
        )?;
        Ok(())
    }

    // ── Player activity helpers ────────────────────────────────────────────

    /// Maximum activity rows kept per server.
    pub const MAX_ACTIVITY_ROWS: usize = 5000;

    /// Record a player join, leave, or death event.
    pub fn activity_append(
        &self,
        server_id: &str,
        event: &str,
        player: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO player_activity (server_id, ts, event, player, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![server_id, now_ms, event, player, detail],
        )?;
        // Prune oldest rows beyond the cap.
        conn.execute(
            "DELETE FROM player_activity
             WHERE server_id = ?1
               AND id NOT IN (
                 SELECT id FROM player_activity
                 WHERE server_id = ?1
                 ORDER BY id DESC LIMIT ?2
               )",
            params![server_id, Self::MAX_ACTIVITY_ROWS as i64],
        )?;
        Ok(())
    }

    /// Load the last `limit` activity rows for a server (oldest first).
    pub fn activity_load(
        &self,
        server_id: &str,
        limit: usize,
    ) -> Result<Vec<PlayerActivityRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, event, player, detail FROM (
                SELECT id, ts, event, player, detail
                FROM player_activity
                WHERE server_id = ?1
                ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![server_id, limit as i64], |r| {
            Ok(PlayerActivityRow {
                ts:     r.get(0)?,
                event:  r.get(1)?,
                player: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

// ── Public types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlayerActivityRow {
    pub ts:     i64,
    pub event:  String,
    pub player: String,
    pub detail: Option<String>,
}
