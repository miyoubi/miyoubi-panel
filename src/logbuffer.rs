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
use std::sync::{Arc, Mutex};
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
    Regex::new(r"^([A-Za-z0-9_.\-]{1,36}) joined the game$").unwrap()
});

/// Player left: "Steve left the game"
static RE_LEAVE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([A-Za-z0-9_.\-]{1,36}) left the game$").unwrap()
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

/// Named villager / mob death — actual Minecraft/Fabric format:
///   Named entity class_3989['Russel'/24, l='ServerLevel[world]', x=25.35, y=62.61, z=525.68] died: Russel was slain by .Yuki67139
/// Captures: (name, dimension, x, y, z, death_message)
static RE_MOB_DEATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^Named entity \S+\['([^']+)'/\d+,\s*l='ServerLevel\[([^\]]+)\]',\s*x=([\d.\-]+),\s*y=([\d.\-]+),\s*z=([\d.\-]+)\] died:\s*(.+)$"
    ).unwrap()
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
    // the name inside the brackets and a JSON detail with dim, coords, and message.
    if let Some(c) = RE_MOB_DEATH.captures(payload) {
        let name   = c[1].to_string();
        let dim    = c[2].to_string();
        let x      = c[3].to_string();
        let y      = c[4].to_string();
        let z      = c[5].to_string();
        let msg    = c[6].to_string();
        // Encode all fields as JSON so the frontend can reconstruct the card
        let detail = serde_json::json!({
            "villager": true,
            "msg": msg,
            "dim": dim,
            "x": x,
            "y": y,
            "z": z
        }).to_string();
        return Some(("villager_death", name, Some(detail)));
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
    /// Set after construction so logbuffer can fire RCON commands without
    /// a circular dependency at build time.
    docker:    std::sync::OnceLock<Arc<crate::docker::DockerClient>>,
}

impl LogBuffer {
    pub fn new(server_id: impl Into<String>, db: crate::db::Db) -> Self {
        let server_id = server_id.into();
        let (tx, _) = broadcast::channel(1024);
        let history = db.log_load(&server_id, MAX_LINES).unwrap_or_default();
        let now = Instant::now();
        let buf = history.into_iter().map(|l| (now, l)).collect::<VecDeque<_>>();
        Self {
            server_id,
            db,
            inner: Mutex::new(Inner { buf }),
            tx,
            docker: std::sync::OnceLock::new(),
        }
    }

    /// Wire up the docker client so that death-location queries can be fired.
    pub fn set_docker(&self, docker: Arc<crate::docker::DockerClient>) {
        let _ = self.docker.set(docker);
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
                // Also write to the per-player death log table.
                if event == "death" {
                    let msg = detail.as_deref().unwrap_or("died");
                    match self.db.death_log_append(&self.server_id, &player, msg) {
                        Ok(row_id) => {
                            // Spawn a task to query LastDeathLocation via RCON
                            // and update the row once we get coords back.
                            if let Some(docker) = self.docker.get().cloned() {
                                let db2       = self.db.clone();
                                let srv_id    = self.server_id.clone();
                                let player2   = player.clone();
                                tokio::spawn(async move {
                                    fetch_and_store_death_location(
                                        docker, db2, srv_id, player2, row_id,
                                    ).await;
                                });
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "death_log_append failed for {} ({}): {}",
                                self.server_id, player, e
                            );
                        }
                    }
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

/// Fired in a background task after each player death.
/// Sends `/data get entity <player> LastDeathLocation` via RCON, parses the
/// coordinates + dimension from the output, then patches the DB row.
///
/// Minecraft output looks like (may vary by version/mod):
///   <player> has the following entity data: {pos: [25.0d, 64.0d, 100.0d], dimension: "minecraft:overworld"}
/// Or for older formats:
///   Data value of <player> is: {dim: 0, pos: [25.0d, 64.0d, 100.0d]}
async fn fetch_and_store_death_location(
    docker:  Arc<crate::docker::DockerClient>,
    db:      crate::db::Db,
    srv_id:  String,
    player:  String,
    row_id:  i64,
) {
    // Small delay so the player is properly dead before we query
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let cmd = format!("data get entity {} LastDeathLocation", player);
    let output = match docker.send_command(&cmd).await {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("LastDeathLocation cmd failed for {}: {}", player, e);
            return;
        }
    };

    // Parse coords: [Xd, Yd, Zd]  (the 'd' suffix is Minecraft's double tag)
    // Handles both integer arrays [I; x, y, z] and doubles [xd, yd, zd]
    let pos_re = Regex::new(r"\[(?:I;\s*)?([+-]?\d+(?:\.\d+)?)[dD]?,\s*([+-]?\d+(?:\.\d+)?)[dD]?,\s*([+-]?\d+(?:\.\d+)?)[dD]?\]").unwrap();
    let coords = if let Some(cap) = pos_re.captures(&output) {
        Some((cap[1].to_string(), cap[2].to_string(), cap[3].to_string()))
    } else {
        tracing::debug!("Could not parse coords from LastDeathLocation output: {}", output);
        None
    };

    // Parse dimension: "minecraft:overworld" → "overworld"
    let dim_re = Regex::new(r#"dimension:\s*"(?:minecraft:)?([^"]+)""#).unwrap();
    let dim = dim_re.captures(&output)
        .map(|c| c[1].to_string());

    if coords.is_none() && dim.is_none() {
        return; // Nothing useful to store
    }

    let coords_str = coords.as_ref().map(|(x, y, z)| format!("{},{},{}", x, y, z));
    if let Err(e) = db.death_log_update_coords(
        &srv_id, row_id, dim.as_deref(), coords_str.as_deref(),
    ) {
        tracing::warn!("death_log_update_coords failed for {} row {}: {}", player, row_id, e);
    }
}
