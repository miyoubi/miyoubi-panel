//! Backup module — manages a `itzg/mc-backup` sidecar container per server.
//!
//! The sidecar runs alongside the Minecraft container using the same compose
//! project. It reads the server's /data volume (read-only) and writes
//! compressed tar archives to a /backups volume on the host.
//!
//! Enabling backup:
//!   • Appends the mc-backup service to docker-compose.yml
//!   • Runs `docker compose up -d` — starts only the new sidecar, leaves MC running
//!
//! Disabling backup:
//!   • Removes the mc-backup service block from docker-compose.yml
//!   • Runs `docker compose up -d --remove-orphans` — stops and removes the sidecar
//!
//! The MC server is never stopped during either operation.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::registry::ServerDef;

// ── Backup config (stored in server.json) ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    /// Whether the backup sidecar is enabled for this server.
    pub enabled: bool,
    /// Cron expression for backup schedule (default: every 2 hours).
    pub cron: String,
    /// How many backup archives to keep before pruning older ones.
    pub keep_count: u32,
    /// Directory on the host where backups are stored.
    pub backup_dir: String,
    /// RCON password — must match RCON_PASSWORD in the MC container.
    pub rcon_password: String,
}

impl BackupConfig {
    pub fn default_for(def: &ServerDef) -> Self {
        BackupConfig {
            enabled: false,
            cron: "0 */2 * * *".to_string(),
            keep_count: 10,
            backup_dir: server_backup_dir(def),
            rcon_password: "changeme".to_string(),
        }
    }
}

// ── Helpers shared between BackupConfig and backup_service_block ─────────

/// Canonical backup directory for a server — always derived from the server's
/// own compose file path so it can never accidentally point to another server.
pub fn server_backup_dir(def: &ServerDef) -> String {
    std::path::Path::new(&def.compose_file)
        .parent()
        .map(|p| format!("{}/backups", p.display()))
        .unwrap_or_else(|| "./backups".to_string())
}

// ── Compose snippet ───────────────────────────────────────────────────────

/// Generates the mc-backup sidecar service block to append to docker-compose.yml.
/// Uses itzg/mc-backup with rsync-based hot backup (no server stop needed).
pub fn backup_service_block(def: &ServerDef, cfg: &BackupConfig) -> String {
    // mc_svc MUST match the service key in the compose file, which registry.rs
    // sets to sanitize(def.name) — NOT the container_name.
    // Using container_name here caused "depends on undefined service" errors.
    let mc_svc     = sanitize_svc(&def.name);
    let svc_name   = format!("{}-backup", mc_svc);
    let data_path  = &def.data_path;
    let backup_dir = &cfg.backup_dir;
    let cron       = &cfg.cron;
    let keep       = cfg.keep_count;
    let rcon_pass  = &cfg.rcon_password;

    // Each line is manually indented:
    //   2 spaces  -> service name (child of `services:`)
    //   4 spaces  -> service-level keys (image, container_name, ...)
    //   6 spaces  -> nested keys (environment entries, volume entries, depends_on items)
    let mut out = String::new();
    out.push_str("\n  # ── mc-backup sidecar ───────────────────────────────────────────────\n");
    out.push_str(&format!("  {svc_name}:\n"));
    out.push_str(&format!("    image: itzg/mc-backup\n"));
    out.push_str(&format!("    container_name: {svc_name}\n"));
    out.push_str(          "    restart: unless-stopped\n");
    out.push_str(          "    depends_on:\n");
    out.push_str(&format!("      - {mc_svc}\n"));
    out.push_str(          "    environment:\n");
    out.push_str(          "      BACKUP_METHOD: tar\n");
    out.push_str(          "      INITIAL_DELAY: \"15\"\n");
    out.push_str(&format!("      BACKUP_INTERVAL: \"{cron}\"\n"));
    out.push_str(          "      PRUNE_BACKUPS_DAYS: \"0\"\n");
    out.push_str(          "      BACKUP_NAME: world\n");
    out.push_str(          "      EXCLUDES: \"*.jar,cache,logs\"\n");
    out.push_str(          "      TAR_COMPRESS_METHOD: gzip\n");
    out.push_str(          "      BACKUP_ON_STARTUP: \"true\"\n");
    out.push_str(&format!("      PRUNE_BACKUP_COUNT: \"{keep}\"\n"));
    out.push_str(&format!("      RCON_HOST: {mc_svc}\n"));
    out.push_str(          "      RCON_PORT: \"25575\"\n");
    out.push_str(&format!("      RCON_PASSWORD: \"{rcon_pass}\"\n"));
    out.push_str(          "      SERVER_PORT: \"25565\"\n");
    out.push_str(          "    volumes:\n");
    out.push_str(&format!("      - {data_path}:/data:ro\n"));
    out.push_str(&format!("      - {backup_dir}:/backups\n"));
    out
}

/// Strip the backup sidecar block from a compose file string.
pub fn strip_backup_block(compose: &str) -> String {
    // The block starts with the comment marker we insert
    if let Some(idx) = compose.find("\n  # ── mc-backup sidecar") {
        compose[..idx].to_string()
    } else {
        compose.to_string()
    }
}

// ── Backup file listing ───────────────────────────────────────────────────

#[derive(Serialize)]
pub struct BackupFile {
    pub name: String,
    pub size_bytes: u64,
    pub modified: String,
}

pub fn list_backups(backup_dir: &str) -> Result<Vec<BackupFile>> {
    let path = std::path::Path::new(backup_dir);
    if !path.exists() {
        return Ok(vec![]);
    }

    let mut files: Vec<BackupFile> = std::fs::read_dir(path)
        .with_context(|| format!("reading backup dir {:?}", backup_dir))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let name = n.to_string_lossy();
            name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".zip")
        })
        .map(|e| {
            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| {
                    let secs = t
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs();
                    Some(format_unix_ts(secs))
                })
                .unwrap_or_else(|| "unknown".to_string());
            BackupFile {
                name: e.file_name().to_string_lossy().to_string(),
                size_bytes: size,
                modified,
            }
        })
        .collect();

    // Newest first
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(files)
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Derive the docker-compose service name for the MC server from its container name.
/// Mirrors `sanitize()` in registry.rs — compose service names must be lowercase alphanumeric + hyphens.
fn mc_svc_name(container_name: &str) -> String {
    sanitize_svc(container_name)
}

fn backup_svc_name(container_name: &str) -> String {
    sanitize_svc(container_name)
}

fn sanitize_svc(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase()
}

fn format_unix_ts(secs: u64) -> String {
    // Simple ISO-ish formatting without chrono dep in this module
    let s = secs;
    let sec   = s % 60;
    let min   = (s / 60) % 60;
    let hour  = (s / 3600) % 24;
    let days  = s / 86400;
    // Days since Unix epoch → approximate date (good enough for display)
    // We use chrono via the registry module, but here we just return the raw ts
    // as a sortable string so the frontend can use its own Date formatting.
    format!("{:010}-{:02}:{:02}:{:02}", days, hour, min, sec)
}
