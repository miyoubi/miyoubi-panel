mod backup;
mod db;
mod docker;
mod files;
mod handlers;
mod logbuffer;
mod registry;
mod setup;
mod users;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{
    http::{header, Method, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use handlers::AppState;
use registry::ServerRegistry;
use setup::TlsConfig;
use db::Db;

#[tokio::main]
async fn main() {
    // rustls 0.23+ requires an explicit CryptoProvider to be installed before
    // any TLS code runs. axum-server and reqwest both use rustls internally;
    // without this call the process panics with "Could not automatically
    // determine the process-level CryptoProvider".
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // ── Config dir ───────────────────────────────────────────────────────
    let config_dir_raw = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "config".to_string());
    let config_dir = {
        let p = PathBuf::from(&config_dir_raw);
        std::fs::create_dir_all(&p).expect("Failed to create config dir");
        std::fs::canonicalize(&p).unwrap_or_else(|_| {
            std::env::current_dir().expect("cannot get cwd").join(&p)
        })
    };
    let config_dir = config_dir.to_string_lossy().to_string();

    // ── Database ─────────────────────────────────────────────────────────
    let db = Db::open(&config_dir).expect("Failed to open panel database");

    // ── TLS wizard ───────────────────────────────────────────────────────
    // If --reconfigure flag passed, clear the TLS row so the wizard re-runs.
    if std::env::args().any(|a| a == "--reconfigure") {
        db.tls_save(&TlsConfig::disabled()).expect("Failed to reset TLS config");
        println!("TLS config reset — re-running setup wizard...");
    }

    let tls_cfg = setup::load_or_run_wizard(&config_dir, &db)
        .await
        .expect("TLS setup failed");

    // ── Registry ─────────────────────────────────────────────────────────
    let registry = ServerRegistry::load(&config_dir, db.clone())
        .expect("Failed to load server registry");
    info!("Loaded {} server(s) from {}", registry.list().len(), config_dir);

    // ── User store ───────────────────────────────────────────────────────
    let users = users::UserStore::new(db.clone());
    // CLI first-run wizard: only prompts if the DB has no users yet AND
    // we're not running in web-setup mode (i.e. the /setup route handles it
    // if someone lands there without accounts). We still support the CLI path
    // for headless / automated deployments.
    if users::UserStore::needs_setup(&db) && std::env::args().any(|a| a == "--cli-setup") {
        run_account_wizard(&users).expect("Failed to create initial admin account");
    }
    info!("User store loaded ({} accounts)", users.list().len());

    let state = AppState { registry, users };

    // ── Build the main app router ─────────────────────────────────────────
    let app = build_router(state);

    // ── Start servers ─────────────────────────────────────────────────────
    if tls_cfg.enabled {
        run_https(app, tls_cfg, config_dir).await;
    } else {
        run_http(app, tls_cfg.http_port, config_dir).await;
    }
}

// ── First-run account creation wizard ────────────────────────────────────────

fn run_account_wizard(store: &users::UserStore) -> anyhow::Result<()> {
    use std::io::{self, Write};

    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║   Miyoubi Panel — Create Admin Account   ║");
    println!("╚══════════════════════════════════════════╝");
    println!();
    println!("  No user accounts found. Set up your admin account to get started.");
    println!();

    let username = loop {
        print!("  Admin username: ");
        io::stdout().flush().unwrap();
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        let name = line.trim().to_string();
        if name.is_empty() {
            println!("  Username cannot be empty.");
            continue;
        }
        if name.contains(':') || name.contains(' ') {
            println!("  Username cannot contain spaces or colons.");
            continue;
        }
        break name;
    };

    let password = loop {
        print!("  Password: ");
        io::stdout().flush().unwrap();
        // Read without echo if possible (rpassword not available — use stdin)
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        let pass = line.trim().to_string();
        if pass.len() < 8 {
            println!("  Password must be at least 8 characters.");
            continue;
        }
        print!("  Confirm password: ");
        io::stdout().flush().unwrap();
        let mut confirm = String::new();
        io::stdin().read_line(&mut confirm).unwrap();
        if pass != confirm.trim() {
            println!("  Passwords do not match.");
            continue;
        }
        break pass;
    };

    println!();
    store.bootstrap(&username, &password)?;
    println!("  ✓ Admin account '{}' created.", username);
    println!();

    Ok(())
}


fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE]);

    Router::new()
        .route("/api/dashboard",            get(handlers::dashboard))
        .route("/api/servers",              get(handlers::servers_list).post(handlers::servers_create))
        .route("/api/servers/:id",          delete(handlers::servers_delete))
        .route("/api/servers/:id/status",   get(handlers::status))
        .route("/api/servers/:id/stats",    get(handlers::stats))
        .route("/api/servers/:id/start",    post(handlers::start))
        .route("/api/servers/:id/stop",     post(handlers::stop))
        .route("/api/servers/:id/restart",  post(handlers::restart))
        .route("/api/servers/:id/logs",       get(handlers::stream_logs))
        .route("/api/servers/:id/logs/clear", post(handlers::logs_clear))
        .route("/api/servers/:id/command",  post(handlers::command))
        .route("/api/servers/:id/players",  get(handlers::players))
        .route("/api/servers/:id/activity", get(handlers::activity_log))
        .route("/api/servers/:id/files",         get(handlers::files_dir))
        .route("/api/servers/:id/files/content",  get(handlers::file_content))
        .route("/api/servers/:id/files/write",    post(handlers::file_write))
        .route("/api/servers/:id/mods",          get(handlers::mods))
        .route("/api/servers/:id/mods/enable",   post(handlers::mod_enable))
        .route("/api/servers/:id/mods/disable",  post(handlers::mod_disable))
        .route("/api/servers/:id/mods/remove",   post(handlers::mod_remove))
        .route("/api/servers/:id/mods/install",  post(handlers::mod_install))
        .route("/api/servers/:id/config",   get(handlers::config_get).post(handlers::config_set))
        .route("/api/servers/:id/opencl",   post(handlers::set_opencl))
        .route("/api/servers/:id/backup",   get(handlers::backup_get).post(handlers::backup_set))
        .route("/favicon.ico",              get(serve_favicon))
        .route("/favicon.png",              get(serve_favicon))
        // ── Auth ─────────────────────────────────────────────────────────
        .route("/api/auth/login",           post(handlers::auth_login))
        .route("/api/auth/logout",          post(handlers::auth_logout))
        .route("/api/auth/me",             get(handlers::auth_me))
        .route("/api/setup",               get(handlers::setup_status).post(handlers::setup_create))
        // ── User management (admin only) ──────────────────────────────────
        .route("/api/users",               get(handlers::users_list).post(handlers::users_create))
        .route("/api/users/:username",     put(handlers::users_update).delete(handlers::users_delete))
        .route("/api/users/:username/servers", put(handlers::users_set_servers))
        .route("/api/users/:username/password", post(handlers::users_change_password))
        // ── Public status (no auth) ───────────────────────────────────────
        .route("/api/public/:id/status",   get(handlers::public_status))
        .route("/api/public/:id/stats",    get(handlers::public_stats))
        .route("/api/public/:id/players",  get(handlers::public_players))
        .route("/s/:id",                   get(handlers::serve_status_page))
        // ── Frontend ──────────────────────────────────────────────────────
        .route("/login",                    get(handlers::serve_login))
        .route("/setup",                    get(handlers::serve_setup))
        .route("/view",                     get(handlers::serve_viewer))
        .route("/dashboard",               get(handlers::serve_dashboard))
        .route("/mobile",                   get(handlers::serve_mobile))
        .route("/server/:id",              get(handlers::serve_frontend))
        .route("/",                         get(handlers::serve_frontend))
        .with_state(state)
        .layer(cors)
}

// ── Plain HTTP mode ───────────────────────────────────────────────────────

async fn run_http(app: Router, port: u16, config_dir: String) {
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
    info!("Minecraft Panel (HTTP) on http://{}  config={}", addr, config_dir);
    info!("Run with --reconfigure to set up HTTPS / Let's Encrypt");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind HTTP port");
    axum::serve(listener, app).await.expect("HTTP server error");
}

// ── HTTPS mode ────────────────────────────────────────────────────────────

async fn run_https(app: Router, tls: TlsConfig, config_dir: String) {
    let https_port = tls.https_port;
    let http_port  = tls.http_port;
    let domain     = tls.domain.clone().unwrap_or_default();
    let cert       = tls.cert.clone().unwrap();
    let key        = tls.key.clone().unwrap();

    // ── Cert-renewal watcher ─────────────────────────────────────────────
    // certbot's deploy hook touches config/cert_renewed when it renews.
    // We watch for that file and reload certs without restarting the process.
    let cfg_dir_clone = config_dir.clone();
    let cert_clone    = cert.clone();
    let key_clone     = key.clone();

    // Shared TLS config for hot-reload
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .expect("Failed to load TLS certificate");

    let tls_cfg_reload = tls_config.clone();
    tokio::spawn(async move {
        watch_cert_renewal(cfg_dir_clone, cert_clone, key_clone, tls_cfg_reload).await;
    });

    // ── HTTP → HTTPS redirect (also handles ACME renewal) ────────────────
    let https_port_clone = https_port;
    let redirect_app = Router::new().fallback(move |uri: Uri, req: axum::http::Request<axum::body::Body>| {
        let host = req.headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
            .unwrap_or_default();
        let path_and_query = uri.path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let target = if https_port_clone == 443 {
            format!("https://{}{}", host, path_and_query)
        } else {
            format!("https://{}:{}{}", host, https_port_clone, path_and_query)
        };
        async move { Redirect::permanent(&target) }
    });

    let http_addr:  SocketAddr = format!("0.0.0.0:{}", http_port).parse().unwrap();
    let https_addr: SocketAddr = format!("0.0.0.0:{}", https_port).parse().unwrap();

    info!("Minecraft Panel (HTTPS) on https://{}  config={}", https_addr, config_dir);
    info!("HTTP on {} → redirects to HTTPS", http_addr);
    info!("Domain: {}  |  Run with --reconfigure to change TLS settings", domain);

    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .expect("failed to bind HTTP redirect port");

    // Spawn HTTP redirect server
    tokio::spawn(async move {
        axum::serve(http_listener, redirect_app)
            .await
            .expect("HTTP redirect server error");
    });

    // HTTPS server (main)
    axum_server::bind_rustls(https_addr, tls_config)
        .serve(app.into_make_service())
        .await
        .expect("HTTPS server error");
}

// ── Certificate hot-reload ────────────────────────────────────────────────
// certbot's deploy hook writes `config/cert_renewed` when it renews a cert.
// We poll for that file every 12 hours and reload the cert if found.

async fn watch_cert_renewal(
    config_dir: String,
    cert: String,
    key: String,
    tls_cfg: axum_server::tls_rustls::RustlsConfig,
) {
    let flag = PathBuf::from(&config_dir).join("cert_renewed");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(12 * 3600)).await;
        if flag.exists() {
            info!("Certificate renewal detected — reloading TLS certs...");
            match tls_cfg.reload_from_pem_file(&cert, &key).await {
                Ok(()) => {
                    info!("TLS certs reloaded successfully");
                    let _ = std::fs::remove_file(&flag);
                }
                Err(e) => tracing::error!("Failed to reload TLS certs: {}", e),
            }
        }
    }
}

// ── Favicon ───────────────────────────────────────────────────────────────

async fn serve_favicon() -> Response {
    for name in &["favicon.png", "favicon.ico", "icon.png", "icon.ico"] {
        if let Ok(data) = std::fs::read(name) {
            let ct = if name.ends_with(".png") { "image/png" } else { "image/x-icon" };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "max-age=86400")],
                data,
            ).into_response();
        }
    }
    let ico: &[u8] = &[
        0,0,1,0,1,0,1,1,0,0,1,0,32,0,40,0,0,0,22,0,0,0,40,0,0,0,1,0,0,0,2,0,
        0,0,1,0,32,0,0,0,0,0,4,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    ];
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/x-icon"), (header::CACHE_CONTROL, "max-age=86400")],
        ico,
    ).into_response()
}
