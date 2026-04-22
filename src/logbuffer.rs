//! Per-server console log buffer.
//!
//! Lines are held in a `VecDeque` in memory for fast SSE streaming and
//! also persisted to the SQLite database (`console_log` table) so history
//! survives restarts. Pressing "Clear" in the UI calls `clear()` which
//! wipes both the in-memory buffer and the database rows — no stale history
//! leaks back on the next page load.
//!
//! Storage is compact: only `kind` (short tag) and `text` are stored;
//! timestamps are Unix-millisecond integers rather than formatted strings.
//! Duplicate lines within a 2-second window are dropped.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use tokio::sync::broadcast;

pub const MAX_LINES: usize = 2000;
const DEDUP_WINDOW: Duration = Duration::from_secs(2);

// ── Activity-event patterns ───────────────────────────────────────────────
//
// Minecraft server thread log lines look like:
//   [HH:MM:SS] [Server thread/INFO]: <message>
//
// We match only the message part after the INFO]: prefix.

/// Matches the INFO prefix so we can strip it and match only the payload.
static RE_INFO_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\[[\d:]+\] \[.*?/INFO\]: (.+)$").unwrap()
});

/// Player joined: "Steve joined the game"
static RE_JOIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(.+) joined the game$").unwrap()
});

/// Player left: "Steve left the game"
static RE_LEAVE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(.+) left the game$").unwrap()
});

/// Player/mob death message — Minecraft emits these as:
///   Steve was slain by Zombie
///   Steve drowned
///   Bart was slain by Zombie           (player names)
/// The server also logs entity deaths for named mobs:
///   Villager class_1646['Bart'/<id>, ...] died, message: '<detail>'
static RE_PLAYER_DEATH: Lazy<Regex> = Lazy::new(|| {
    // Matches lines starting with a player name (no spaces, letters/digits/
    // underscores) followed by a vanilla death-message verb.
    Regex::new(
        r"^(\S+) (died|was shot|was pricked|walked into|was killed|drowned|blew up|was blown up|was knocked|hit the ground|fell|was impaled|was squished|was fireballed|was stung|was struck|burned to death|went up in flames|tried to swim|was slain|suffocated|was pummeled|starved|was frozen|was poked|experienced kinetic|didn't want to live|discovered the floor|went off|was obliterated|left the confines)"
    ).unwrap()
});

/// Named villager / mob death:
///   Villager class_1646['Bart'/70, l='...', x=…] died, message: 'Bart was slain by Zombie'
static RE_MOB_DEATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^\S+\['([^']+)'/\d+.*\] died, message: '(.+)'$"#).unwrap()
});

/// Parse a raw console line and return `Some((event, player, detail))` if it
/// matches a trackable activity event, or `None` otherwise.
fn parse_activity(line: &str) -> Option<(&'static str, String, Option<String>)> {
    // Strip the [HH:MM:SS] [Server thread/INFO]: prefix.
    let payload = RE_INFO_PREFIX.captures(line)?.get(1)?.as_str();

    if let Some(c) = RE_JOIN.captures(payload) {
        return Some(("join", c[1].to_string(), None));
    }
    if let Some(c) = RE_LEAVE.captures(payload) {
        return Some(("leave", c[1].to_string(), None));
    }
    // Named mob death (e.g. named villager) — produces a death entry using
    // the name inside the brackets and the full death message as detail.
    if let Some(c) = RE_MOB_DEATH.captures(payload) {
        return Some(("death", c[1].to_string(), Some(c[2].to_string())));
    }
    // Player death — player name + death verb, detail is the full message.
    if let Some(c) = RE_PLAYER_DEATH.captures(payload) {
        return Some(("death", c[1].to_string(), Some(payload.to_string())));
    }
    None
}

// ── Types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub text: String,
    pub kind: String,
}

// ── Buffer ────────────────────────────────────────────────────────────────

struct Inner {
    buf: VecDeque<(Instant, LogLine)>,
}

pub struct LogBuffer {
    server_id: String,
    db:        crate::db::Db,
    inner:     Mutex<Inner>,
    tx:        broadcast::Sender<LogLine>,
}

impl LogBuffer {
    /// Create a new buffer for `server_id`, loading existing history from the DB.
    pub fn new(server_id: impl Into<String>, db: crate::db::Db) -> Self {
        let server_id = server_id.into();
        let (tx, _) = broadcast::channel(1024);

        // Load persisted history.
        let history = db.log_load(&server_id, MAX_LINES).unwrap_or_default();
        let now = Instant::now();
        let buf = history.into_iter()
            .map(|l| (now, l))
            .collect::<VecDeque<_>>();

        Self {
            server_id,
            db,
            inner: Mutex::new(Inner { buf }),
            tx,
        }
    }

    /// Push a line: sanitise → dedup → store in memory + DB → broadcast.
    pub fn push(&self, text: String, kind: &str) {
        self.push_inner(text, kind, true);
    }

    /// Like `push`, but skips activity-event detection.
    /// Use this when replaying historical docker log lines so we don't
    /// re-insert duplicate player_activity rows on every panel restart.
    pub fn push_history(&self, text: String, kind: &str) {
        self.push_inner(text, kind, false);
    }

    fn push_inner(&self, text: String, kind: &str, record_activity: bool) {
        // Sanitise: split on bare \r, keep last non-empty segment,
        // strip non-printable control chars (keep tab).
        let text = {
            let last = text.split('\r')
                .filter(|s| !s.trim().is_empty())
                .last()
                .unwrap_or(&text)
                .to_string();
            last.chars()
                .filter(|&c| c == '\t' || (c as u32 >= 0x20 && c as u32 != 0x7F))
                .collect::<String>()
        };
        let text = text.trim().to_string();
        if text.is_empty() { return; }

        let now = Instant::now();
        let line = LogLine { text: text.clone(), kind: kind.to_string() };

        {
            let mut inner = self.inner.lock().unwrap();

            // Dedup: drop if identical text seen within the window.
            let is_dup = inner.buf.iter().rev().any(|(t, l)| {
                now.duration_since(*t) < DEDUP_WINDOW && l.text == text
            });
            if is_dup { return; }

            if inner.buf.len() >= MAX_LINES {
                inner.buf.pop_front();
            }
            inner.buf.push_back((now, line.clone()));
        }

        // Persist outside the lock so the DB write doesn't block broadcasts.
        if let Err(e) = self.db.log_append(&self.server_id, kind, &text) {
            tracing::warn!("log_append failed for {}: {}", self.server_id, e);
        }

        // Detect player activity events (join / leave / death) and persist
        // them to the dedicated player_activity table.
        if record_activity && kind == "docker" {
            if let Some((event, player, detail)) = parse_activity(&text) {
                if let Err(e) = self.db.activity_append(
                    &self.server_id, event, &player, detail.as_deref(),
                ) {
                    tracing::warn!(
                        "activity_append failed for {} ({} {}): {}",
                        self.server_id, event, player, e
                    );
                }
            }
        }

        let _ = self.tx.send(line);
    }

    /// Snapshot of all in-memory lines (oldest first), for SSE replay.
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner.lock().unwrap()
            .buf.iter()
            .map(|(_, l)| l.clone())
            .collect()
    }

    /// Subscribe to future lines.
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    /// Expose the underlying DB handle so callers can run activity queries.
    pub fn db_ref(&self) -> &crate::db::Db {
        &self.db
    }

    /// Clear both the in-memory buffer and the persisted DB rows.
    /// After this, `snapshot()` returns `[]` and a fresh page load
    /// will start with an empty console.
    pub fn clear(&self) {
        self.inner.lock().unwrap().buf.clear();
        if let Err(e) = self.db.log_clear(&self.server_id) {
            tracing::warn!("log_clear failed for {}: {}", self.server_id, e);
        }
    }
}
