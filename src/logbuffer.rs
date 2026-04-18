use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const MAX_LINES: usize = 2000;
const DEDUP_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub text: String,
    pub kind: String,
}

struct Inner {
    buf: VecDeque<(Instant, LogLine)>,
    log_file: Option<File>,
}

pub struct LogBuffer {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<LogLine>,
}

impl LogBuffer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                log_file: None,
            }),
            tx,
        }
    }

    /// Read existing lines from `path` into the buffer, then keep the file
    /// open for future appends. File format: `kind\ttext\n` per line.
    pub fn load_from_file(&self, path: &str) -> Result<()> {
        // Read history first.
        let rf = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path))?;

        let mut lines: Vec<(Instant, LogLine)> = Vec::new();
        for raw in BufReader::new(&rf).lines() {
            let raw = raw?;
            if raw.is_empty() {
                continue;
            }
            if let Some(tab) = raw.find('\t') {
                let text = raw[tab + 1..]
                    .chars()
                    .filter(|&c| c == '\t' || (c as u32 >= 0x20 && c as u32 != 0x7F))
                    .collect::<String>();
                let text = text.trim().to_string();
                if text.is_empty() { continue; }
                lines.push((
                    Instant::now(),
                    LogLine {
                        kind: raw[..tab].to_string(),
                        text,
                    },
                ));
            }
        }
        drop(rf);

        // Trim to max.
        if lines.len() > MAX_LINES {
            lines.drain(0..lines.len() - MAX_LINES);
        }

        // If file is over-full, rewrite trimmed version.
        let needs_rewrite = lines.len() == MAX_LINES;

        {
            let mut inner = self.inner.lock().unwrap();
            inner.buf = lines.into_iter().collect();
        }

        if needs_rewrite {
            self.rewrite_file(path);
        }

        // Re-open for appending.
        let wf = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("opening log file for append: {}", path))?;

        self.inner.lock().unwrap().log_file = Some(wf);
        Ok(())
    }

    fn rewrite_file(&self, path: &str) {
        let inner = self.inner.lock().unwrap();
        if let Ok(mut f) = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(path)
        {
            for (_, line) in &inner.buf {
                let _ = writeln!(f, "{}\t{}", line.kind, line.text);
            }
        }
    }

    /// Push a line into the buffer, persist it, and broadcast to all subscribers.
    /// Lines with identical text within the dedup window are silently dropped —
    /// this prevents double-delivery when rcon output also appears in Docker logs.
    pub fn push(&self, text: String, kind: &str) {
        // ── Sanitize ─────────────────────────────────────────────────────
        // Docker attach (and apt-get progress output) can produce lines with
        // embedded bare \r (carriage return without \n). These are used by
        // terminals to overwrite the current line. Split on \r and take the
        // last non-empty segment — that's the final state of the line.
        // Then strip any remaining ASCII control characters (except tab).
        // This prevents the axum SSE panic:
        //   "SSE field value cannot contain newlines or carriage returns"
        let text = {
            let last = text.split('\r')
                .filter(|s| !s.trim().is_empty())
                .last()
                .unwrap_or(&text)
                .to_string();
            // Strip non-printable control chars (keep tab=0x09, keep >=0x20)
            last.chars()
                .filter(|&c| c == '\t' || (c as u32 >= 0x20 && c as u32 != 0x7F))
                .collect::<String>()
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        let now = Instant::now();

        let line = LogLine {
            text: text.clone(),
            kind: kind.to_string(),
        };

        {
            let mut inner = self.inner.lock().unwrap();

            // Dedup check: scan recent entries backwards.
            let is_dup = inner.buf.iter().rev().any(|(t, l)| {
                now.duration_since(*t) < DEDUP_WINDOW && l.text == text
            });
            if is_dup {
                return;
            }

            if inner.buf.len() >= MAX_LINES {
                inner.buf.pop_front();
            }
            inner.buf.push_back((now, line.clone()));

            if let Some(ref mut f) = inner.log_file {
                let _ = writeln!(f, "{}\t{}", kind, text);
            }
        }

        // Broadcast outside the lock — slow subscribers just get RecvError::Lagged.
        let _ = self.tx.send(line);
    }

    /// Return a point-in-time snapshot of all buffered lines.
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .unwrap()
            .buf
            .iter()
            .map(|(_, l)| l.clone())
            .collect()
    }

    /// Subscribe to future log lines.
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    pub fn close(&self) {
        self.inner.lock().unwrap().log_file.take();
    }
}

impl Drop for LogBuffer {
    fn drop(&mut self) {
        self.close();
    }
}
