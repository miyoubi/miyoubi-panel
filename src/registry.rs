//! Server registry — manages multiple Minecraft server instances.
//!
//! On-disk layout:
//!   config/servers/{name}-{id}/
//!     server.json          ← ServerDef metadata
//!     docker-compose.yml   ← editable compose file
//!     data/                ← bind-mounted into the container as /data
//!     console.log          ← per-server console history

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backup::{self, BackupConfig};
use crate::docker::DockerClient;
use crate::logbuffer::LogBuffer;

// ── On-disk metadata ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDef {
    pub id: String,
    pub name: String,
    pub container_name: String,
    pub data_path: String,
    pub compose_file: String,
    pub port: u16,
    pub created_at: DateTime<Utc>,
    /// Whether this server has OpenCL/GPU support enabled via the gpu.sh entrypoint.
    #[serde(default)]
    pub opencl_enabled: bool,
    /// Backup sidecar configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_config: Option<BackupConfig>,
}

// ── Runtime instance ──────────────────────────────────────────────────────

pub struct ServerInstance {
    pub def: ServerDef,
    pub docker: Arc<DockerClient>,
    pub log_buffer: Arc<LogBuffer>,
}

// ── Registry ──────────────────────────────────────────────────────────────

pub struct ServerRegistry {
    servers:    RwLock<HashMap<String, Arc<ServerInstance>>>,
    config_dir: PathBuf,
    db:         crate::db::Db,
}

impl ServerRegistry {
    pub fn load(config_dir: impl Into<PathBuf>, db: crate::db::Db) -> Result<Arc<Self>> {
        let config_dir = config_dir.into();
        let servers_dir = config_dir.join("servers");
        fs::create_dir_all(&servers_dir).context("creating servers dir")?;

        let reg = Arc::new(Self {
            servers:    RwLock::new(HashMap::new()),
            config_dir,
            db:         db.clone(),
        });

        for entry in fs::read_dir(&servers_dir).context("reading servers dir")? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let json_path = entry.path().join("server.json");
            let data = match fs::read_to_string(&json_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let def: ServerDef = match serde_json::from_str(&data) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Skipping {:?}: {}", entry.path(), e);
                    continue;
                }
            };

            let id = def.id.clone();
            // If compose_file or data_path are relative (from old server.json),
            // make them absolute relative to the servers directory so they work
            // regardless of where the binary is launched from.
            let def = fix_paths(def, &servers_dir);
            match Self::build_instance(def, reg.db.clone()) {
                Ok(inst) => {
                    let inst = Arc::new(inst);
                    reg.servers.write().unwrap().insert(id.clone(), inst.clone());
                    tokio::spawn(Self::pump_logs(inst));
                }
                Err(e) => tracing::warn!("Could not init server {}: {}", id, e),
            }
        }

        Ok(reg)
    }

    fn build_instance(def: ServerDef, db: crate::db::Db) -> Result<ServerInstance> {
        let docker = Arc::new(
            DockerClient::new(def.container_name.clone())
                .context("connecting to Docker")?,
        );
        // LogBuffer loads history from the DB automatically.
        let lb = Arc::new(LogBuffer::new(def.id.clone(), db));
        // Give the buffer a handle to docker so it can query LastDeathLocation.
        lb.set_docker(docker.clone());
        Ok(ServerInstance { def, docker, log_buffer: lb })
    }

    /// Continuously stream Docker logs into the buffer.
    /// - "No such container" (404) → container not created yet, wait quietly.
    /// - Clean stream end → container stopped normally, replay 500 lines on reconnect.
    /// - Other error → log it, replay 500 lines on reconnect so history is preserved.
    async fn pump_logs(inst: Arc<ServerInstance>) {
        let mut tail = "500";
        loop {
            match inst.docker.stream_logs_to_buffer(&inst.log_buffer, tail).await {
                Ok(()) => {
                    // Container stopped cleanly — keep logs in DB (user can
                    // read them while offline). When the server starts again,
                    // the start/restart handler calls clear() before pump_logs
                    // reconnects, so we use tail="0" here to avoid re-delivering
                    // the previous session's lines.
                    tail = "0";
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    let msg = e.to_string();

                    // Container hasn't been started yet — expected, wait quietly.
                    let is_no_container = msg.contains("No such container")
                        || msg.contains("no such container")
                        || msg.contains("404");

                    // Idle timeout: Minecraft can be silent for many minutes.
                    // The stream_client has timeout=0 but Docker daemon itself
                    // may close idle connections. This is normal — reconnect
                    // silently with tail="0" (don't replay history again).
                    let is_timeout = msg.contains("Timeout")
                        || msg.contains("timeout")
                        || msg.contains("timed out");

                    // Connection reset when container restarts — normal.
                    let is_reset = msg.contains("connection reset")
                        || msg.contains("broken pipe")
                        || msg.contains("unexpected end")
                        || msg.contains("hyper");

                    if is_no_container {
                        tracing::debug!(
                            "[{}] Container not running yet, retrying in 5s",
                            inst.def.name
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    } else if is_timeout {
                        // Silent reconnect — don't replay history, don't warn.
                        tracing::debug!(
                            "[{}] Log stream idle timeout, reconnecting",
                            inst.def.name
                        );
                        tail = "0";
                        // No sleep — reconnect immediately.
                        continue;
                    } else if is_reset {
                        tracing::debug!(
                            "[{}] Log stream reset (container restart?), retrying",
                            inst.def.name
                        );
                        // The restart handler calls clear() before we get here,
                        // so tail="0" avoids re-streaming cleared history.
                        tail = "0";
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    } else {
                        // Unknown error from attach. Before warning, check whether
                        // the container is actually running — if it's offline there's
                        // no point attaching and we should wait quietly.
                        let is_running = inst.docker.get_status().await.running;
                        if is_running {
                            tracing::warn!(
                                "[{}] Log stream error: {} — retrying in 5s",
                                inst.def.name, e
                            );
                        } else {
                            tracing::debug!(
                                "[{}] Log stream error while container offline, waiting: {}",
                                inst.def.name, e
                            );
                        }
                        tail = "0";
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    // ── Queries ───────────────────────────────────────────────────────────

    pub fn list(&self) -> Vec<ServerDef> {
        self.servers
            .read()
            .unwrap()
            .values()
            .map(|i| i.def.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<ServerInstance>> {
        self.servers.read().unwrap().get(id).cloned()
    }

    // ── Create ────────────────────────────────────────────────────────────

    pub async fn create(self: &Arc<Self>, req: CreateRequest) -> Result<ServerDef> {
        if req.name.trim().is_empty() {
            anyhow::bail!("name is required");
        }

        let id = rand_id();
        let slug = sanitize(&req.name);
        let dir_name = format!("{}-{}", slug, id);
        // Always use absolute path derived from config_dir (already canonicalized)
        let dir = self.config_dir.join("servers").join(&dir_name);
        let data_dir = dir.join("data");

        fs::create_dir_all(&data_dir).context("creating server data dir")?;

        let container_name = dir_name.clone();
        let compose_path = dir.join("docker-compose.yml");

        let def = ServerDef {
            id: id.clone(),
            name: req.name.clone(),
            container_name: container_name.clone(),
            // Store absolute paths so the server works regardless of cwd on restart
            data_path: data_dir.to_string_lossy().to_string(),
            compose_file: compose_path.to_string_lossy().to_string(),
            port: req.port.unwrap_or(25565),
            created_at: Utc::now(),
            opencl_enabled: req.opencl_enabled.unwrap_or(false),
            backup_config: None,
        };

        let json = serde_json::to_string_pretty(&def)?;
        fs::write(dir.join("server.json"), &json).context("writing server.json")?;

        // Write gpu.sh into data/ so it is bind-mounted into the container at /data/gpu.sh.
        // The entrypoint runs `/data/gpu.sh && /start` inside the container.
        // We always write it so toggling OpenCL on later works without recreating the server.
        let gpu_sh_host = data_dir.join("gpu.sh");
        fs::write(&gpu_sh_host, GPU_SH).context("writing gpu.sh")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&gpu_sh_host, fs::Permissions::from_mode(0o755));
        }

        let compose = generate_compose(&def, &req);
        fs::write(&compose_path, &compose).context("writing docker-compose.yml")?;

        let inst = Arc::new(Self::build_instance(def.clone(), self.db.clone())?);
        self.servers.write().unwrap().insert(id.clone(), inst.clone());
        tokio::spawn(Self::pump_logs(inst));

        Ok(def)
    }

    // ── Delete ────────────────────────────────────────────────────────────

    pub fn delete(&self, id: &str) -> Result<()> {
        let inst = self
            .servers
            .write()
            .unwrap()
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("server not found"))?;

        let dir = Path::new(&inst.def.compose_file)
            .parent()
            .map(PathBuf::from);

        drop(inst);

        if let Some(d) = dir {
            fs::remove_dir_all(&d)
                .with_context(|| format!("removing {:?}", d))?;
        }
        Ok(())
    }

    // ── docker compose control ────────────────────────────────────────────
    //
    // We use the `docker compose` CLI (not bollard ContainerStart) because:
    //  • ContainerStart needs the container to already exist — it 404s on first start.
    //  • `docker compose up -d` creates + starts in one command.
    //  • It respects the compose file the user edited.

    pub async fn compose_up(inst: &ServerInstance) -> Result<()> {
        run_compose(&inst.def.compose_file, &["up", "-d", "--remove-orphans"]).await
    }

    pub async fn compose_stop(inst: &ServerInstance) -> Result<()> {
        run_compose(&inst.def.compose_file, &["stop"]).await
    }

    pub async fn compose_restart(inst: &ServerInstance) -> Result<()> {
        run_compose(&inst.def.compose_file, &["restart"]).await
    }

    // ── Compose file access ───────────────────────────────────────────────

    pub fn compose_read(&self, id: &str) -> Result<String> {
        let inst = self.get(id).ok_or_else(|| anyhow::anyhow!("server not found"))?;
        fs::read_to_string(&inst.def.compose_file)
            .with_context(|| format!("reading {:?}", inst.def.compose_file))
    }

    pub fn compose_write(&self, id: &str, content: &str) -> Result<()> {
        let inst = self.get(id).ok_or_else(|| anyhow::anyhow!("server not found"))?;
        fs::write(&inst.def.compose_file, content)
            .with_context(|| format!("writing {:?}", inst.def.compose_file))
    }

    /// Toggle OpenCL/GPU support on an existing server.
    /// Updates server.json and rewrites the compose file (if it uses our template).
    pub fn set_opencl(&self, id: &str, enabled: bool) -> Result<()> {
        let inst = self.get(id).ok_or_else(|| anyhow::anyhow!("server not found"))?;

        // Update the in-memory def
        // We need to persist the change to server.json
        let mut def = inst.def.clone();
        def.opencl_enabled = enabled;

        // Rewrite server.json
        let json_path = std::path::Path::new(&def.compose_file)
            .parent().unwrap().join("server.json");
        let json = serde_json::to_string_pretty(&def)?;
        fs::write(&json_path, &json).context("writing server.json")?;

        // Rewrite compose file only if it still looks like our template
        let current = fs::read_to_string(&def.compose_file).unwrap_or_default();
        if current.contains("AUTOPAUSE_ENABLED") {
            // Parse out what we can from the existing compose
            let new_compose = regenerate_compose_for_def(&def, &current);
            fs::write(&def.compose_file, &new_compose)
                .with_context(|| format!("rewriting {:?}", def.compose_file))?;
        }

        // Ensure gpu.sh exists in data/ — needed if this is the first time OpenCL
        // is enabled and the server was created before we started writing it there.
        if enabled {
            let data_dir = std::path::Path::new(&def.data_path);
            let gpu_sh_host = data_dir.join("gpu.sh");
            if !gpu_sh_host.exists() {
                fs::write(&gpu_sh_host, GPU_SH).context("writing gpu.sh to data dir")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&gpu_sh_host, fs::Permissions::from_mode(0o755));
                }
            }
        }

        tracing::info!("[{}] OpenCL set to {}", def.name, enabled);
        Ok(())
    }

    /// Enable or update the backup sidecar for a server.
    /// Appends (or replaces) the mc-backup service block in docker-compose.yml
    /// and runs `docker compose up -d` to start it without touching the MC container.
    ///
    /// Disabling removes the block and runs `up -d --remove-orphans` to stop the sidecar.
    pub fn set_backup(&self, id: &str, mut cfg: BackupConfig) -> Result<()> {
        let inst = self.get(id).ok_or_else(|| anyhow::anyhow!("server not found"))?;
        let mut def = inst.def.clone();

        // Always override backup_dir with the canonical path for THIS server.
        // The frontend may send a stale or wrong path if the user never edited it,
        // or if a previously broken default pointed at a different server's directory.
        cfg.backup_dir = backup::server_backup_dir(&def);

        // Create backup dir if enabling
        if cfg.enabled {
            fs::create_dir_all(&cfg.backup_dir)
                .with_context(|| format!("creating backup dir {:?}", cfg.backup_dir))?;
        }

        // Read current compose, strip any existing backup block, then re-append if enabling
        let current = fs::read_to_string(&def.compose_file)
            .with_context(|| format!("reading {:?}", def.compose_file))?;
        let stripped = backup::strip_backup_block(&current);
        let new_compose = if cfg.enabled {
            format!("{}{}", stripped, backup::backup_service_block(&def, &cfg))
        } else {
            stripped
        };

        fs::write(&def.compose_file, &new_compose)
            .with_context(|| format!("writing {:?}", def.compose_file))?;

        // Persist config into server.json
        def.backup_config = Some(cfg.clone());
        let json_path = std::path::Path::new(&def.compose_file)
            .parent().unwrap().join("server.json");
        let json = serde_json::to_string_pretty(&def)?;
        fs::write(&json_path, &json).context("writing server.json")?;

        tracing::info!("[{}] Backup sidecar set to enabled={}", def.name, cfg.enabled);
        Ok(())
    }

    /// Return the effective backup config for a server (defaulting if never set).
    pub fn get_backup_config(&self, id: &str) -> Result<BackupConfig> {
        let inst = self.get(id).ok_or_else(|| anyhow::anyhow!("server not found"))?;
        Ok(inst.def.backup_config.clone()
            .unwrap_or_else(|| BackupConfig::default_for(&inst.def)))
    }
} // end impl ServerRegistry

// ── Helpers ───────────────────────────────────────────────────────────────

/// Make any relative paths in a ServerDef absolute, using servers_dir as the base.
/// Needed for server.json files created before absolute paths were stored.
fn fix_paths(mut def: ServerDef, servers_dir: &std::path::Path) -> ServerDef {
    let make_abs = |p: &str| -> String {
        let path = std::path::Path::new(p);
        if path.is_absolute() {
            p.to_string()
        } else {
            // The relative path is relative to where the binary runs from,
            // but we can also reconstruct it from servers_dir + dir_name
            servers_dir
                .join(p)
                .to_string_lossy()
                .to_string()
        }
    };
    def.compose_file = make_abs(&def.compose_file);
    def.data_path    = make_abs(&def.data_path);
    def
}

/// Run `docker compose <args>` with the compose file's directory as cwd.
/// Uses the absolute compose_file path stored in ServerDef.
pub async fn run_compose_pub(compose_file: &str, args: &[&str]) -> Result<()> {
    run_compose(compose_file, args).await
}

async fn run_compose(compose_file: &str, args: &[&str]) -> Result<()> {
    let dir = Path::new(compose_file)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid compose file path: {}", compose_file))?;

    let output = tokio::process::Command::new("docker")
        .arg("compose")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("running docker compose {} in {:?}", args.join(" "), dir))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "docker compose {} failed:\n{}\n{}",
            args.join(" "),
            stderr.trim(),
            stdout.trim()
        );
    }
    Ok(())
}

// ── Request types ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    pub port: Option<u16>,
    pub server_type: Option<String>,
    pub version: Option<String>,
    pub memory: Option<String>,
    /// Run `docker compose up -d` immediately after creating.
    pub start_now: Option<bool>,
    /// Generate compose with OpenCL/GPU support (nvidia runtime + gpu.sh entrypoint).
    pub opencl_enabled: Option<bool>,
}

// ── ID / name helpers ─────────────────────────────────────────────────────

fn rand_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    format!("{:08x}", rng.gen::<u32>())
}

fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            'A'..='Z' => out.push(c.to_ascii_lowercase()),
            ' ' | '-' | '_' => {
                if !out.ends_with('-') {
                    out.push('-');
                }
            }
            _ => {}
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.len() > 20 { out[..20].to_string() } else { out }
}

// ── gpu.sh content ────────────────────────────────────────────────────────

const GPU_SH: &str = r#"#!/bin/sh
# Install OpenCL support for NVIDIA GPUs
apt-get update && apt-get install -y \
    ocl-icd-libopencl1 \
    opencl-headers \
    clinfo \
    && rm -rf /var/lib/apt/lists/*

mkdir -p /etc/OpenCL/vendors && \
    echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd
"#;

// ── Compose template ──────────────────────────────────────────────────────

fn generate_compose(def: &ServerDef, req: &CreateRequest) -> String {
    let server_type = req.server_type.as_deref().unwrap_or("FABRIC");
    let version = req.version.as_deref().unwrap_or("LATEST");
    let memory = req.memory.as_deref().unwrap_or("2G");
    let port = def.port;
    let container = &def.container_name;
    let data_path = &def.data_path;
    let svc = sanitize(&def.name);
    let opencl = req.opencl_enabled.unwrap_or(false);

    // These sections are only emitted when OpenCL/GPU is enabled
    let runtime_line       = if opencl { "    runtime: nvidia\n" } else { "" };
    // exec /start replaces the bash shell so the java process becomes PID 1.
    // Docker captures PID 1's stdout/stderr directly — without exec, java is a
    // grandchild of bash and its output can get buffered or lost in the log driver.
    let entrypoint_line    = if opencl { "    entrypoint: [\"/bin/bash\", \"-c\", \"bash /data/gpu.sh && exec /start\"]\n" } else { "" };
    let gpu_deploy         = if opencl {
        "    deploy:\n      resources:\n        reservations:\n          devices:\n            - driver: nvidia\n              count: 1\n              capabilities: [gpu]\n"
    } else { "" };
    let nvidia_env         = if opencl {
        "      NVIDIA_VISIBLE_DEVICES: all\n      NVIDIA_DRIVER_CAPABILITIES: compute,utility,graphics,video\n"
    } else { "" };

    format!(
        "services:\n  {svc}:\n    image: itzg/minecraft-server\n    container_name: {container}\n{runtime_line}{entrypoint_line}{gpu_deploy}    stdin_open: true\n    environment:\n      EULA: \"true\"\n      TYPE: \"{server_type}\"\n      VERSION: \"{version}\"\n      MAX_MEMORY: \"{memory}\"\n      ENABLE_RCON: \"true\"\n      RCON_PASSWORD: \"changeme\"\n      DIFFICULTY: \"normal\"\n      MODE: \"survival\"\n      MOTD: \"A Minecraft Server\"\n      AUTOPAUSE_ENABLED: \"false\"\n      MAX_TICK_TIME: \"-1\"\n      SERVER_PORT: \"{port}\"\n{nvidia_env}    ports:\n      - \"{port}:{port}\"\n    volumes:\n      - {data_path}:/data\n    restart: unless-stopped\n"
    )
}

/// Regenerate compose YAML preserving server_type/version/memory from existing file.
/// Simply re-uses generate_compose with a synthetic CreateRequest built from the def.
fn regenerate_compose_for_def(def: &ServerDef, existing: &str) -> String {
    let server_type = extract_env(existing, "TYPE").unwrap_or_else(|| "FABRIC".into());
    let version     = extract_env(existing, "VERSION").unwrap_or_else(|| "LATEST".into());
    let memory      = extract_env(existing, "MAX_MEMORY").unwrap_or_else(|| "2G".into());

    let req = CreateRequest {
        name:           def.name.clone(),
        port:           Some(def.port),
        server_type:    Some(server_type),
        version:        Some(version),
        memory:         Some(memory),
        start_now:      None,
        opencl_enabled: Some(def.opencl_enabled),
    };
    generate_compose(def, &req)
}

fn extract_env(compose: &str, key: &str) -> Option<String> {
    for line in compose.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(key) {
            if let Some(rest) = trimmed.strip_prefix(key) {
                let val = rest.trim_start_matches([':', ' ', '=', '"']).trim_matches('"');
                if !val.is_empty() { return Some(val.to_string()); }
            }
        }
    }
    None
}

/// Legacy: kept for compatibility, calls regenerate_compose_for_def.
#[allow(dead_code)]
pub fn regenerate_compose(def: &ServerDef) -> Result<()> {
    let current = std::fs::read_to_string(&def.compose_file).unwrap_or_default();
    if current.contains("AUTOPAUSE_ENABLED") {
        let new_compose = regenerate_compose_for_def(def, &current);
        std::fs::write(&def.compose_file, &new_compose)
            .with_context(|| format!("rewriting {:?}", def.compose_file))?;
    }
    Ok(())
}
