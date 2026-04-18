use anyhow::{Context, Result};
use bollard::container::{
    AttachContainerOptions, InspectContainerOptions, LogOutput, LogsOptions, StatsOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::Docker;
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

use crate::logbuffer::LogBuffer;

// ── RCON noise filter ─────────────────────────────────────────────────────

const RCON_NOISE: &[&str] = &["RCON Listener", "Thread RCON Client", "RCON Client #"];

pub fn is_rcon_noise(line: &str) -> bool {
    RCON_NOISE.iter().any(|p| line.contains(p))
}

// ── Types ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Default, Clone)]
pub struct ServerStatus {
    pub running: bool,
    pub status: String,
    pub container_id: String,
    pub image: String,
    pub uptime: String,
    pub cpu_percent: f64,
    pub mem_usage_mb: f64,
    pub mem_limit_mb: f64,
}

#[derive(Serialize, Default, Clone)]
pub struct PlayerList {
    pub online: Vec<String>,
    pub count: usize,
    pub max: usize,
}

// ── DockerClient ──────────────────────────────────────────────────────────

pub struct DockerClient {
    /// Short-timeout client for API calls (inspect, start, stop, exec, stats).
    api: Docker,
    /// No-timeout client dedicated to log streaming. Log streams can be idle
    /// for many minutes — sharing with API calls poisons the connection pool
    /// when the stream times out, causing status polls to fail and flash OFFLINE.
    stream_client: Docker,
    pub container_name: String,
}

impl DockerClient {
    pub fn new(container_name: String) -> Result<Self> {
        let socket_path = std::env::var("DOCKER_HOST")
            .ok()
            .and_then(|h| h.strip_prefix("unix://").map(str::to_string))
            .unwrap_or_else(|| "/var/run/docker.sock".to_string());

        // 15s timeout is plenty for any individual Docker API call.
        let api = Docker::connect_with_socket(&socket_path, 15, bollard::API_DEFAULT_VERSION)
            .context("connecting to Docker daemon (api)")?;

        // No timeout for streaming — Minecraft can be completely silent for minutes.
        let stream_client = Docker::connect_with_socket(&socket_path, 0, bollard::API_DEFAULT_VERSION)
            .context("connecting to Docker daemon (stream)")?;

        Ok(Self { api, stream_client, container_name })
    }

    // ── Status ────────────────────────────────────────────────────────────
    //
    // get_status does NOT fetch stats (CPU/mem). Stats require a 1-second
    // blocking sample from Docker which makes every status poll take ≥1s and
    // causes the 15-second docker_call timeout to fire intermittently.
    // Stats are fetched separately by get_stats() which is called on its own
    // schedule and fills the same ServerStatus fields.

    pub async fn get_status(&self) -> ServerStatus {
        let info = match self
            .api
            .inspect_container(&self.container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!(
                    "inspect_container({}) failed: {}",
                    self.container_name, e
                );
                return ServerStatus {
                    running: false,
                    status: "not found".into(),
                    ..Default::default()
                };
            }
        };

        let id = info.id.as_deref().unwrap_or_default();
        let short_id = if id.len() >= 12 { &id[..12] } else { id };
        let state = info.state.as_ref();

        // running = true only when Docker reports the container is actively running.
        let running = state.and_then(|s| s.running).unwrap_or(false);

        // Convert the bollard enum to a plain lowercase string.
        // ContainerStateStatusEnum::Running -> "running", etc.
        let status_str = state
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{:?}", s).trim_matches('"').to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());

        let image = info
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .unwrap_or_default();

        let uptime = if running {
            state
                .and_then(|s| s.started_at.as_ref())
                .and_then(|t| t.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .map(|dt| format_uptime(chrono::Utc::now() - dt))
                .unwrap_or_default()
        } else {
            String::new()
        };

        tracing::debug!(
            "get_status({}) -> running={} status={}",
            self.container_name, running, status_str
        );

        ServerStatus {
            running,
            status: status_str,
            container_id: short_id.to_string(),
            image,
            uptime,
            ..Default::default()
        }
    }

    /// Fetch CPU/memory stats. Returns quickly (one-shot sample).
    /// Called separately so it doesn't slow down the status poll.
    pub async fn get_stats(&self, container_id: &str) -> Option<(f64, f64, f64)> {
        let mut stream = self.api.stats(
            container_id,
            Some(StatsOptions { stream: false, one_shot: true }),
        );
        if let Some(Ok(stats)) = stream.next().await {
            let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as f64
                - stats.precpu_stats.cpu_usage.total_usage as f64;
            let sys_delta = stats.cpu_stats.system_cpu_usage.unwrap_or(0) as f64
                - stats.precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
            let num_cpus = stats
                .cpu_stats
                .online_cpus
                .or_else(|| {
                    stats.cpu_stats.cpu_usage.percpu_usage
                        .as_ref()
                        .map(|v| v.len() as u64)
                })
                .unwrap_or(1) as f64;
            let cpu = if sys_delta > 0.0 && cpu_delta > 0.0 {
                (cpu_delta / sys_delta) * num_cpus * 100.0
            } else {
                0.0
            };
            let mem_mb = stats.memory_stats.usage.unwrap_or(0) as f64 / 1_048_576.0;
            let lim_mb = stats.memory_stats.limit.unwrap_or(0) as f64 / 1_048_576.0;
            Some((cpu, mem_mb, lim_mb))
        } else {
            None
        }
    }

    // ── Log streaming ─────────────────────────────────────────────────────

    pub async fn stream_logs_to_buffer(&self, lb: &LogBuffer, tail: &str) -> Result<()> {
        // ── Phase 1: historical lines via `docker logs` ───────────────────
        // `logs()` reliably returns past output even from TTY containers.
        // We use follow=false here so we just drain history and move on.
        if tail != "0" {
            let mut hist = self.stream_client.logs(
                &self.container_name,
                Some(LogsOptions::<String> {
                    stdout: true,
                    stderr: true,
                    follow: false,
                    timestamps: false,
                    tail: tail.to_string(),
                    ..Default::default()
                }),
            );
            while let Some(msg) = hist.next().await {
                if let Ok(LogOutput::StdOut { message })
                    | Ok(LogOutput::StdErr { message })
                    | Ok(LogOutput::Console { message }) = msg
                {
                    let text = String::from_utf8_lossy(&message);
                    // Split on both \n and \r so terminal-overwrite lines
                    // (apt-get progress etc.) become separate entries.
                    // push() will further sanitize and deduplicate.
                    for line in text.split(|c| c == '\n' || c == '\r') {
                        let line = line.trim();
                        if !line.is_empty() && !is_rcon_noise(line) {
                            lb.push(line.to_string(), "docker");
                        }
                    }
                }
            }
        }

        // ── Phase 2: live output via `docker attach` ──────────────────────
        // `logs(follow=true)` stops capturing after exec() chains in TTY
        // containers — exactly the symptoms seen with the OpenCL entrypoint
        // (gpu.sh output shows, Minecraft server output does not).
        // `attach_container` connects to the container's stdio multiplexer
        // directly, mirroring `docker attach`, and works regardless of how
        // many times the container process calls exec().
        let attach = self
            .stream_client
            .attach_container(
                &self.container_name,
                Some(AttachContainerOptions::<String> {
                    stdout: Some(true),
                    stderr: Some(true),
                    stdin:  Some(false),
                    stream: Some(true),
                    // logs=false — we already loaded history in phase 1
                    logs:   Some(false),
                    ..Default::default()
                }),
            )
            .await
            .context("attaching to container output stream")?;

        let mut output = attach.output;
        while let Some(msg) = output.next().await {
            match msg {
                Ok(LogOutput::StdOut { message })
                | Ok(LogOutput::StdErr { message })
                | Ok(LogOutput::Console { message }) => {
                    let text = String::from_utf8_lossy(&message);
                    for line in text.split(|c| c == '\n' || c == '\r') {
                        let line = line.trim();
                        if !line.is_empty() && !is_rcon_noise(line) {
                            lb.push(line.to_string(), "docker");
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!("{}", e));
                }
            }
        }

        Ok(())
    }

    // ── Commands ──────────────────────────────────────────────────────────

    pub async fn send_command(&self, command: &str) -> Result<String> {
        let exec = self
            .api
            .create_exec(
                &self.container_name,
                CreateExecOptions::<String> {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(vec!["rcon-cli".to_string(), command.to_string()]),
                    ..Default::default()
                },
            )
            .await
            .context("creating exec")?;

        let mut output = String::new();

        match self.api.start_exec(&exec.id, None).await.context("starting exec")? {
            StartExecResults::Attached { output: mut exec_stream, .. } => {
                while let Some(msg) = exec_stream.next().await {
                    match msg.context("reading exec output")? {
                        LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                            output.push_str(&String::from_utf8_lossy(&message));
                        }
                        _ => {}
                    }
                }
            }
            StartExecResults::Detached => {}
        }

        Ok(output.trim().to_string())
    }

    // ── Players ───────────────────────────────────────────────────────────

    pub async fn get_players(&self) -> Result<PlayerList> {
        match self.send_command("list").await {
            Ok(output) => Ok(parse_player_list(&output)),
            Err(_) => Ok(PlayerList::default()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn format_uptime(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 { format!("{}h {}m {}s", h, m, sec) }
    else if m > 0 { format!("{}m {}s", m, sec) }
    else { format!("{}s", sec) }
}

static PLAYER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"There are (\d+) of a max(?: of)? (\d+) players online:(.*)").unwrap()
});

fn parse_player_list(output: &str) -> PlayerList {
    if let Some(caps) = PLAYER_RE.captures(output) {
        let count: usize = caps[1].parse().unwrap_or(0);
        let max: usize = caps[2].parse().unwrap_or(20);
        let names_str = caps[3].trim();
        let online: Vec<String> = if names_str.is_empty() {
            vec![]
        } else {
            names_str.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        PlayerList { online, count, max }
    } else {
        PlayerList::default()
    }
}
