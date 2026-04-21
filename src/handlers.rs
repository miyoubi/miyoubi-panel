use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Json, Response,
    },
};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::files;
use crate::registry::{CreateRequest, ServerRegistry};
use crate::backup::{BackupConfig, list_backups};
use anyhow::Context as AnyhowContext;

// ── App state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<ServerRegistry>,
    pub users:    crate::users::UserStore,
    pub db:       crate::db::Db,
}

// ── Error handling ────────────────────────────────────────────────────────

pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({"ok": false, "message": self.0.to_string()});
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self { ApiError(e.into()) }
}

type ApiResult<T> = Result<T, ApiError>;

// Use a background-context timeout so browser cancellation doesn't abort
// in-flight Docker operations and produce spurious errors.
async fn docker_call<F, T>(f: F) -> Result<T, ApiError>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .map_err(|_| ApiError(anyhow::anyhow!("Docker operation timed out")))
}

// ── Frontend ──────────────────────────────────────────────────────────────

static INDEX_HTML:     &str = include_str!("../static/index.html");
static LOGIN_HTML:     &str = include_str!("../static/login.html");
static VIEWER_HTML:    &str = include_str!("../static/viewer.html");
static MOBILE_HTML:    &str = include_str!("../static/mobile.html");
static SETUP_HTML:     &str = include_str!("../static/setup.html");

pub async fn serve_frontend()  -> Html<&'static str> { Html(INDEX_HTML) }
pub async fn serve_login()     -> Html<&'static str> { Html(LOGIN_HTML) }
pub async fn serve_viewer()    -> Html<&'static str> { Html(VIEWER_HTML) }
pub async fn serve_mobile() -> Html<&'static str> { Html(MOBILE_HTML) }

pub async fn serve_dashboard(
    headers: axum::http::HeaderMap,
) -> Html<&'static str> {
    // Serve the mobile UI when the client is a phone or narrow-screen device
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_mobile = ua.contains("Mobile") || ua.contains("Android")
        || ua.contains("iPhone") || ua.contains("iPad");
    if is_mobile { Html(MOBILE_HTML) } else { Html(INDEX_HTML) }
}
pub async fn serve_setup()     -> Html<&'static str> { Html(SETUP_HTML) }

// ── Server list & create ──────────────────────────────────────────────────

pub async fn servers_list(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let all = s.registry.list();
    // If the caller is a viewer with a server allowlist, filter to only those servers.
    if let Some(user) = cookie_user(&headers) {
        if matches!(s.users.role_of(&user), Some(crate::users::UserRole::Viewer)) {
            if let Some(allowed) = s.users.allowed_servers(&user) {
                let filtered: Vec<_> = all.into_iter()
                    .filter(|srv| allowed.contains(&srv.id))
                    .collect();
                return Json(filtered).into_response();
            }
        }
    }
    Json(all).into_response()
}

pub async fn servers_create(
    State(s): State<AppState>,
    Json(req): Json<CreateRequest>,
) -> ApiResult<impl IntoResponse> {
    let start_now = req.start_now.unwrap_or(false);
    let def = s.registry.create(req).await?;

    if start_now {
        if let Some(inst) = s.registry.get(&def.id) {
            match ServerRegistry::compose_up(&inst).await {
                Ok(()) => inst.log_buffer.push("[panel] Server starting...".into(), "panel"),
                Err(e) => tracing::warn!("start_now failed for {}: {}", def.id, e),
            }
        }
    }

    Ok((StatusCode::CREATED, Json(def)))
}

pub async fn servers_delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    s.registry.delete(&id)?;
    Ok(Json(serde_json::json!({"ok": true, "message": "server deleted"})))
}

// ── Per-server helper ─────────────────────────────────────────────────────

macro_rules! get_server {
    ($state:expr, $id:expr) => {
        match $state.registry.get(&$id) {
            Some(inst) => inst,
            None => return Err(ApiError(anyhow::anyhow!("server not found"))),
        }
    };
}

// ── Status — fast path, no stats ─────────────────────────────────────────
//
// get_status() only calls inspect_container which is instant.
// Stats (CPU/mem) are fetched by the separate /stats endpoint so they don't
// slow down the 5-second status poll and cause spurious timeout→offline flips.

pub async fn status(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let st = docker_call(inst.docker.get_status()).await?;
    Ok(Json(st))
}

/// GET /api/servers/:id/stats — CPU & memory, fetched on demand.
/// Frontend calls this separately after confirming the server is running.
pub async fn stats(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    // We need the real container ID, not the name, for stats
    let st = docker_call(inst.docker.get_status()).await?;
    if !st.running || st.container_id.is_empty() {
        return Ok(Json(serde_json::json!({
            "cpu_percent": 0, "mem_usage_mb": 0, "mem_limit_mb": 0
        })));
    }
    // get_stats uses the short container ID returned by inspect
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        inst.docker.get_stats(&inst.docker.container_name),
    ).await.unwrap_or(None);

    match result {
        Some((cpu, mem, lim)) => Ok(Json(serde_json::json!({
            "cpu_percent": cpu, "mem_usage_mb": mem, "mem_limit_mb": lim
        }))),
        None => Ok(Json(serde_json::json!({
            "cpu_percent": 0, "mem_usage_mb": 0, "mem_limit_mb": 0
        }))),
    }
}

// ── Control — uses docker compose, not bollard ContainerStart ────────────

pub async fn start(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    ServerRegistry::compose_up(&inst).await?;
    inst.log_buffer.clear();
    inst.log_buffer.push("[panel] Server starting...".into(), "panel");
    Ok(Json(serde_json::json!({"ok": true, "message": "server starting"})))
}

pub async fn stop(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    ServerRegistry::compose_stop(&inst).await?;
    inst.log_buffer.push("[panel] Server stopping...".into(), "panel");
    Ok(Json(serde_json::json!({"ok": true, "message": "server stopping"})))
}

pub async fn restart(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    ServerRegistry::compose_restart(&inst).await?;
    inst.log_buffer.clear();
    inst.log_buffer.push("[panel] Server restarting...".into(), "panel");
    Ok(Json(serde_json::json!({"ok": true, "message": "server restarting"})))
}

// ── Console SSE ───────────────────────────────────────────────────────────

// POST /api/servers/:id/logs/clear — wipes both in-memory buffer and DB rows.
// The next SSE snapshot will be empty; pressing Clear in the UI calls this.
pub async fn logs_clear(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if cookie_user(&headers).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"ok": false}))).into_response();
    }
    let inst = match s.registry.get(&id) {
        Some(i) => i,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "message": "server not found"}))).into_response(),
    };
    inst.log_buffer.clear();
    Json(serde_json::json!({"ok": true})).into_response()
}

pub async fn stream_logs(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>> {
    let inst = get_server!(s, id);

    // Subscribe before snapshot to close the race window.
    let mut rx = inst.log_buffer.subscribe();
    let snapshot = inst.log_buffer.snapshot();

    let event_stream = stream! {
        // 2KB padding flushes Cloudflare/nginx proxy buffers immediately.
        yield Ok::<Event, Infallible>(Event::default().comment(" ".repeat(2048)));

        for line in snapshot {
            yield Ok(Event::default().data(line.text));
        }

        loop {
            match rx.recv().await {
                Ok(line) => yield Ok(Event::default().data(line.text)),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("SSE subscriber lagged by {} messages", n);
                    continue;
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(25))
            .text("heartbeat"),
    ))
}

// ── Command ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CommandBody { pub command: String }

pub async fn command(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CommandBody>,
) -> ApiResult<impl IntoResponse> {
    if body.command.is_empty() {
        return Err(ApiError(anyhow::anyhow!("command must not be empty")));
    }
    let inst = get_server!(s, id);
    let output = docker_call(inst.docker.send_command(&body.command)).await??;
    for line in output.lines() {
        let line = line.trim();
        if !line.is_empty() {
            inst.log_buffer.push(line.to_string(), "rcon");
        }
    }
    Ok(Json(serde_json::json!({"ok": true, "message": "command sent"})))
}

// ── Players ───────────────────────────────────────────────────────────────

pub async fn players(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let pl = docker_call(inst.docker.get_players()).await??;
    Ok(Json(pl))
}

// ── File browser ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PathQuery { pub path: Option<String> }

pub async fn files_dir(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let req = q.path.unwrap_or_else(|| "/data".to_string());
    let real = files::map_data_path(&req, &inst.def.data_path);
    let safe = files::safe_path(&inst.def.data_path, &real)?;
    Ok(Json(files::list_dir(&safe)?))
}

pub async fn file_content(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let req = q.path.unwrap_or_default();
    let real = files::map_data_path(&req, &inst.def.data_path);
    let safe = files::safe_path(&inst.def.data_path, &real)?;
    Ok(Json(files::read_file(&safe)?))
}

#[derive(Deserialize)]
pub struct FileWriteBody { pub path: String, pub content: String }

pub async fn file_write(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FileWriteBody>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let real = files::map_data_path(&body.path, &inst.def.data_path);
    let safe = files::safe_path(&inst.def.data_path, &real)?;
    files::write_file(&safe, &body.content)?;
    Ok(Json(serde_json::json!({"ok": true, "message": "file saved"})))
}

// ── Mods ──────────────────────────────────────────────────────────────────

pub async fn mods(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let mods_path = format!("{}/mods", inst.def.data_path);
    Ok(Json(files::list_dir(&mods_path)?))
}

#[derive(Deserialize)]
pub struct ModPathBody { pub path: String }

pub async fn mod_enable(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ModPathBody>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let mod_path = files::map_data_path(&body.path, &inst.def.data_path);
    let file_name = std::path::Path::new(&mod_path)
        .file_name()
        .ok_or_else(|| ApiError(anyhow::anyhow!("invalid path")))?
        .to_string_lossy().to_string();
    std::fs::rename(&mod_path, format!("{}/mods/{}", inst.def.data_path, file_name))?;
    Ok(Json(serde_json::json!({"ok": true, "message": "mod enabled"})))
}

pub async fn mod_disable(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ModPathBody>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let mod_path = files::map_data_path(&body.path, &inst.def.data_path);
    files::disable_mod(&mod_path, &inst.def.data_path)?;
    Ok(Json(serde_json::json!({"ok": true, "message": "mod disabled"})))
}

pub async fn mod_remove(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ModPathBody>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let mod_path = files::map_data_path(&body.path, &inst.def.data_path);
    std::fs::remove_file(&mod_path)?;
    Ok(Json(serde_json::json!({"ok": true, "message": "mod removed"})))
}


// ── Modrinth mod install ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModInstallBody {
    /// Direct download URL from Modrinth (or any HTTPS source).
    pub url: String,
    /// Filename to save as inside the server's mods/ directory.
    pub filename: String,
}

/// POST /api/servers/:id/mods/install
/// Downloads a mod JAR from a URL and saves it into the server's mods/ folder.
/// The frontend calls Modrinth's API to find the URL, then asks us to fetch it —
/// this avoids CORS issues and keeps all file I/O on the server side.
pub async fn mod_install(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ModInstallBody>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);

    // Validate filename — must end in .jar and contain no path separators.
    let fname = body.filename.trim();
    if !fname.ends_with(".jar") {
        return Err(ApiError(anyhow::anyhow!("filename must end in .jar")));
    }
    if fname.contains('/') || fname.contains('\\') || fname.contains("..") {
        return Err(ApiError(anyhow::anyhow!("invalid filename")));
    }

    let mods_dir = format!("{}/mods", inst.def.data_path);
    std::fs::create_dir_all(&mods_dir)
        .context("creating mods directory")?;

    let dest_path = format!("{}/{}", mods_dir, fname);

    // Download the file.
    let client = reqwest::Client::builder()
        .user_agent("minecraft-panel/1.0 (https://github.com)")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("building HTTP client")?;

    let response = client
        .get(&body.url)
        .send()
        .await
        .context("downloading mod")?;

    if !response.status().is_success() {
        return Err(ApiError(anyhow::anyhow!("download failed: HTTP {}", response.status())));
    }

    let bytes = response.bytes().await.context("reading download")?;
    std::fs::write(&dest_path, &bytes)
        .with_context(|| format!("writing mod to {:?}", dest_path))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Installed {}", fname),
        "filename": fname,
    })))
}

// ── Docker-compose config ─────────────────────────────────────────────────

pub async fn config_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let content = s.registry.compose_read(&id)?;
    Ok(Json(serde_json::json!({"filename": "docker-compose.yml", "content": content})))
}

#[derive(Deserialize)]
pub struct ConfigBody { pub content: String }

pub async fn config_set(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConfigBody>,
) -> ApiResult<impl IntoResponse> {
    s.registry.compose_write(&id, &body.content)?;
    Ok(Json(serde_json::json!({"ok": true, "message": "saved docker-compose.yml"})))
}

// ── OpenCL toggle ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OpenClBody {
    pub enabled: bool,
}

/// POST /api/servers/:id/opencl
/// Body: { "enabled": true|false }
/// Rewrites server.json + docker-compose.yml to add/remove GPU support.
/// The server must be stopped and `docker compose up -d` re-run to apply.
pub async fn set_opencl(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<OpenClBody>,
) -> ApiResult<impl IntoResponse> {
    s.registry.set_opencl(&id, body.enabled)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": if body.enabled {
            "OpenCL enabled — stop and restart the server to apply"
        } else {
            "OpenCL disabled — stop and restart the server to apply"
        }
    })))
}

// ── Backup ────────────────────────────────────────────────────────────────

/// GET /api/servers/:id/backup
/// Returns current backup config + list of existing backup files.
pub async fn backup_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let cfg = s.registry.get_backup_config(&id)?;
    let files = list_backups(&cfg.backup_dir).unwrap_or_default();
    Ok(Json(serde_json::json!({
        "config": cfg,
        "files": files,
    })))
}

/// POST /api/servers/:id/backup
/// Body: BackupConfig JSON — enables/updates or disables the backup sidecar.
/// The Minecraft server keeps running; only the sidecar is affected.
pub async fn backup_set(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(cfg): Json<BackupConfig>,
) -> ApiResult<impl IntoResponse> {
    let enabled = cfg.enabled;
    s.registry.set_backup(&id, cfg)?;

    // Apply the compose change live — start or stop the sidecar.
    // We do this in a background task so the HTTP response returns immediately.
    // compose up -d only starts new/changed services; the MC container is unaffected.
    if let Some(inst) = s.registry.get(&id) {
        let compose_file = inst.def.compose_file.clone();
        let server_name  = inst.def.name.clone();
        tokio::spawn(async move {
            let args: &[&str] = if enabled {
                &["up", "-d"]
            } else {
                &["up", "-d", "--remove-orphans"]
            };
            match crate::registry::run_compose_pub(&compose_file, args).await {
                Ok(()) => tracing::info!("[{}] Backup sidecar compose applied", server_name),
                Err(e) => tracing::warn!("[{}] Backup compose failed: {}", server_name, e),
            }
        });
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": if enabled {
            "Backup sidecar enabled — starting in background"
        } else {
            "Backup sidecar disabled — stopping in background"
        }
    })))
}

// ── User management ───────────────────────────────────────────────────────

use crate::users::UserRole;
use axum::http::HeaderMap;

/// Extract username from the `mcpanel_user` cookie.
pub fn cookie_user(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("mcpanel_user=") {
            if !val.is_empty() {
                // URL-decode
                let decoded = urlencoding::decode(val).ok()?.into_owned();
                return Some(decoded);
            }
        }
    }
    None
}

// POST /api/auth/login  { username, password }
// Sets an HttpOnly session cookie on success — the browser stores it
// automatically and it cannot be read by JavaScript.
pub async fn auth_login(
    State(s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Block login entirely until the first-run setup is complete.
    if s.users.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "message": "Setup required", "setup_required": true})),
        ).into_response();
    }
    let username = body["username"].as_str().unwrap_or_default();
    let password = body["password"].as_str().unwrap_or_default();
    match s.users.authenticate(username, password) {
        Some(role) => {
            // Encode the username for the cookie value
            let cookie_val = urlencoding::encode(username).into_owned();
            // HttpOnly + SameSite=Strict prevents JS access and CSRF
            let cookie = format!(
                "mcpanel_user={}; Path=/; Max-Age=604800; HttpOnly; SameSite=Strict",
                cookie_val
            );
            (
                StatusCode::OK,
                [(axum::http::header::SET_COOKIE, cookie)],
                Json(serde_json::json!({
                    "ok": true,
                    "username": username,
                    "role": role.to_string(),
                })),
            ).into_response()
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"ok": false, "message": "Invalid credentials"})),
        ).into_response(),
    }
}

// POST /api/auth/logout — clears the session cookie
pub async fn auth_logout() -> impl IntoResponse {
    let cookie = "mcpanel_user=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict";
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
}

// GET /api/auth/me  — returns logged-in user info from cookie
pub async fn auth_me(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // If no users exist at all, signal setup_required
    if s.users.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"ok": false, "setup_required": true})),
        ).into_response();
    }
    if let Some(user) = cookie_user(&headers) {
        if let Some(role) = s.users.role_of(&user) {
            let allowed = s.users.allowed_servers(&user);
            return Json(serde_json::json!({
                "ok": true,
                "username": user,
                "role": role.to_string(),
                "allowed_servers": allowed,
            })).into_response();
        }
    }
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"ok": false, "message": "Not logged in"}))).into_response()
}

// GET /api/users  — admin only
pub async fn users_list(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !is_admin(&s, &headers) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    Json(s.users.list()).into_response()
}

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username: String,
    pub password: String,
    pub role: String,
}

// POST /api/users
pub async fn users_create(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserBody>,
) -> impl IntoResponse {
    if !is_admin(&s, &headers) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    let role = match body.role.as_str() {
        "viewer" => UserRole::Viewer,
        _ => UserRole::Admin,
    };
    match s.users.create(&body.username, &body.password, role) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateUserBody {
    pub password: Option<String>,
    pub role: Option<String>,
}

// PUT /api/users/:username
pub async fn users_update(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> impl IntoResponse {
    if !is_admin(&s, &headers) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    let role = body.role.as_deref().and_then(|r| match r {
        "viewer" => Some(UserRole::Viewer),
        "admin" => Some(UserRole::Admin),
        _ => None,
    });
    match s.users.update(&username, body.password.as_deref(), role) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":e.to_string()}))).into_response(),
    }
}

// POST /api/users/:username/password — self-service password change.
// Any authenticated user can change their own password.
// Admins can change anyone's password.
pub async fn users_change_password(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let caller = match cookie_user(&headers) {
        Some(u) => u,
        None => return (StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"ok":false,"message":"Not logged in"}))).into_response(),
    };
    // Only allow changing your own password unless you're an admin
    let caller_role = s.users.role_of(&caller);
    let is_admin = matches!(caller_role, Some(UserRole::Admin));
    if caller != username && !is_admin {
        return (StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    let new_pass = match body.get("password").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"message":"Password required"}))).into_response(),
    };
    match s.users.update(&username, Some(&new_pass), None) {
        Ok(()) => Json(serde_json::json!({"ok": true, "message": "Password updated"})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"message":e.to_string()}))).into_response(),
    }
}

// DELETE /api/users/:username
pub async fn users_delete(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> impl IntoResponse {
    if !is_admin(&s, &headers) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    match s.users.delete(&username) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":e.to_string()}))).into_response(),
    }
}

// PUT /api/users/:username/servers — set allowed server IDs for a viewer.
// Body: { "server_ids": ["id1","id2"] } or { "server_ids": null } for all.
pub async fn users_set_servers(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if !is_admin(&s, &headers) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"ok":false,"message":"Forbidden"}))).into_response();
    }
    let ids: Option<Vec<String>> = match body.get("server_ids") {
        Some(serde_json::Value::Array(arr)) => {
            Some(arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        }
        Some(serde_json::Value::Null) | None => None,
        _ => return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"message":"server_ids must be an array or null"}))).into_response(),
    };
    match s.users.set_allowed_servers(&username, ids) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":e.to_string()}))).into_response(),
    }
}


// ── Dashboard — global summary across all servers ─────────────────────────
//
// Returns:
//   servers:    [ { id, name, port, running, uptime, status } ]
//   total:      total server count
//   online:     count of running servers
//   players:    total online players (best-effort, skipped if server offline)
//   host:       { mem_total_mb, mem_used_mb, mem_pct, load1, load5, load15 }

pub async fn dashboard(
    State(s): State<AppState>,
) -> impl IntoResponse {
    let defs = s.registry.list();

    // Collect per-server status concurrently with a short timeout each.
    let mut server_rows = Vec::with_capacity(defs.len());
    let mut online_count: u32 = 0;
    let mut total_players: u32 = 0;

    for def in &defs {
        let status = if let Some(inst) = s.registry.get(&def.id) {
            match tokio::time::timeout(
                std::time::Duration::from_secs(4),
                inst.docker.get_status(),
            ).await {
                Ok(st) => {
                    if st.running { online_count += 1; }
                    // Best-effort player count via RCON — skip if offline/slow
                    if st.running {
                        if let Ok(Ok(pl)) = tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            inst.docker.get_players(),
                        ).await {
                            total_players += pl.count as u32;
                        }
                    }
                    st
                }
                Err(_) => crate::docker::ServerStatus::default(),
            }
        } else {
            crate::docker::ServerStatus::default()
        };

        server_rows.push(serde_json::json!({
            "id":      def.id,
            "name":    def.name,
            "port":    def.port,
            "running": status.running,
            "status":  status.status,
            "uptime":  status.uptime,
        }));
    }

    // ── Host system stats (Linux /proc — zero extra deps) ─────────────────
    let host = read_host_stats();

    Json(serde_json::json!({
        "servers": server_rows,
        "total":   defs.len(),
        "online":  online_count,
        "players": total_players,
        "host":    host,
    })).into_response()
}

fn read_host_stats() -> serde_json::Value {
    // Memory from /proc/meminfo
    let (mem_total, mem_avail) = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .fold((0u64, 0u64), |(t, a), line| {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("MemTotal:")     => (parts.next().and_then(|v| v.parse().ok()).unwrap_or(t), a),
                Some("MemAvailable:") => (t, parts.next().and_then(|v| v.parse().ok()).unwrap_or(a)),
                _ => (t, a),
            }
        });

    let mem_total_mb = mem_total / 1024;
    let mem_used_mb  = mem_total.saturating_sub(mem_avail) / 1024;
    let mem_pct      = if mem_total > 0 { mem_used_mb as f64 / mem_total_mb as f64 * 100.0 } else { 0.0 };

    // Load averages from /proc/loadavg
    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut la = loadavg.split_whitespace();
    let load1  = la.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let load5  = la.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let load15 = la.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);

    // CPU count for normalising load (optional nice-to-have)
    let cpu_count = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count()
        .max(1);

    serde_json::json!({
        "mem_total_mb": mem_total_mb,
        "mem_used_mb":  mem_used_mb,
        "mem_pct":      mem_pct,
        "load1":        load1,
        "load5":        load5,
        "load15":       load15,
        "cpu_count":    cpu_count,
    })
}

// ── First-run setup ───────────────────────────────────────────────────────
//
// GET  /api/setup  — returns { needs_setup: true/false }
// POST /api/setup  { username, password } — creates the initial admin account.
//                   Only works when no users exist. Locked out once setup is done.

pub async fn setup_status(
    State(s): State<AppState>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "needs_setup": s.users.is_empty() }))
}

#[derive(Deserialize)]
pub struct SetupBody {
    pub username: String,
    pub password: String,
}

pub async fn setup_create(
    State(s): State<AppState>,
    Json(body): Json<SetupBody>,
) -> impl IntoResponse {
    // Once any users exist this endpoint is permanently disabled.
    if !s.users.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "message": "Setup already completed"})),
        ).into_response();
    }

    let username = body.username.trim();
    let password = body.password.trim();

    if username.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "message": "Username and password are required"})),
        ).into_response();
    }
    if password.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "message": "Password must be at least 8 characters"})),
        ).into_response();
    }

    match s.users.create(username, password, crate::users::UserRole::Admin) {
        Ok(()) => {
            tracing::info!("First-run setup complete — admin account '{}' created", username);
            // Log the user in immediately by setting the session cookie
            let cookie_val = urlencoding::encode(username).into_owned();
            let cookie = format!(
                "mcpanel_user={}; Path=/; Max-Age=604800; HttpOnly; SameSite=Strict",
                cookie_val
            );
            (
                StatusCode::OK,
                [(axum::http::header::SET_COOKIE, cookie)],
                Json(serde_json::json!({"ok": true, "username": username, "role": "admin"})),
            ).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "message": e.to_string()})),
        ).into_response(),
    }
}


fn is_admin(s: &AppState, headers: &HeaderMap) -> bool {
    cookie_user(headers)
        .and_then(|u| s.users.role_of(&u))
        .map(|r| r == UserRole::Admin)
        .unwrap_or(false)
}

// ── Public status page ────────────────────────────────────────────────────

static STATUS_HTML: &str = include_str!("../static/status.html");

pub async fn serve_status_page() -> Html<&'static str> {
    Html(STATUS_HTML)
}

// GET /api/public/:id/status  — no auth required, read-only
pub async fn public_status(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let inst = match s.registry.get(&id) {
        Some(i) => i,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"not found"}))).into_response(),
    };
    let st = docker_call(inst.docker.get_status()).await.unwrap_or_default();
    // Add server name to public response
    let mut val = serde_json::to_value(&st).unwrap_or_default();
    if let Some(obj) = val.as_object_mut() {
        obj.insert("name".into(), serde_json::Value::String(inst.def.name.clone()));
    }
    Json(val).into_response()
}

// GET /api/public/:id/stats
pub async fn public_stats(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let inst = match s.registry.get(&id) {
        Some(i) => i,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"not found"}))).into_response(),
    };
    // Fetch live stats (CPU + memory). Use a short timeout so a slow Docker
    // daemon doesn't block the public status page for more than a few seconds.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        inst.docker.get_stats(&inst.docker.container_name),
    ).await.unwrap_or(None);

    match result {
        Some((cpu, mem, lim)) => Json(serde_json::json!({
            "cpu_percent":  cpu,
            "mem_usage_mb": mem,
            "mem_limit_mb": lim,
        })).into_response(),
        None => Json(serde_json::json!({
            "cpu_percent":  0.0,
            "mem_usage_mb": 0.0,
            "mem_limit_mb": 0.0,
        })).into_response(),
    }
}

// GET /api/public/:id/players
pub async fn public_players(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let inst = get_server!(s, id);
    let pl = docker_call(inst.docker.get_players()).await??;
    Ok(Json(pl))
}
