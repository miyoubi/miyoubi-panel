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

use tokio::sync::broadcast;

pub const MAX_LINES: usize = 2000;
const DEDUP_WINDOW: Duration = Duration::from_secs(2);

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
