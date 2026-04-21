//! First-run TLS setup wizard.
//!
//! On first launch (no tls.json in config dir), asks the user interactively
//! whether they want HTTPS and runs certbot to obtain a certificate.
//! Saves the result to `{config_dir}/tls.json` so subsequent launches are automatic.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Persisted TLS config ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub enabled:  bool,
    pub domain:   Option<String>,
    pub cert:     Option<String>, // absolute path to fullchain.pem
    pub key:      Option<String>, // absolute path to privkey.pem
    pub http_port:  u16,          // port for HTTP (ACME redirect) or plain HTTP
    pub https_port: u16,          // port for HTTPS
}

impl TlsConfig {
    pub fn disabled() -> Self {
        Self {
            enabled:    false,
            domain:     None,
            cert:       None,
            key:        None,
            http_port:  80,
            https_port: 443,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────

/// Called at startup. Returns the TlsConfig to use.
/// Loads from the database if a config exists, otherwise runs the interactive wizard.
pub async fn load_or_run_wizard(config_dir: &str, db: &crate::db::Db) -> Result<TlsConfig> {
    if let Some(cfg) = db.tls_load()? {
        if cfg.enabled {
            let cert_ok = cfg.cert.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false);
            let key_ok  = cfg.key.as_deref().map(|p| Path::new(p).exists()).unwrap_or(false);
            if cert_ok && key_ok {
                tracing::info!("TLS: loaded config for {:?}", cfg.domain);
                return Ok(cfg);
            }
            eprintln!();
            eprintln!("⚠  TLS cert files not found — re-running TLS setup.");
            eprintln!();
        } else {
            return Ok(cfg);
        }
    }

    // First run or cert missing — interactive wizard
    run_wizard(config_dir, db).await
}

async fn run_wizard(config_dir: &str, db: &crate::db::Db) -> Result<TlsConfig> {
    print_banner();

    let want_https = prompt_bool(
        "Do you want to enable HTTPS with a Let's Encrypt certificate?",
        true,
    );

    if !want_https {
        let cfg = TlsConfig::disabled();
        db.tls_save(&cfg)?;
        println!();
        println!("✓ Running without HTTPS on port 80.");
        println!("  Run with --reconfigure to change this later.");
        println!();
        return Ok(cfg);
    }

    // Ensure certbot is installed
    if !certbot_available() {
        eprintln!();
        eprintln!("✗ certbot not found. Install it first:");
        eprintln!("    sudo apt install certbot");
        eprintln!();
        eprintln!("  Then re-run the panel to continue setup.");
        eprintln!("  (Or answer 'no' to HTTPS to skip for now)");
        std::process::exit(1);
    }

    let domain = prompt_text("Enter your domain name (e.g. panel.example.com):");
    if domain.is_empty() {
        eprintln!("No domain entered — aborting.");
        std::process::exit(1);
    }

    let email = prompt_text("Enter your email for Let's Encrypt notifications:");

    let http_port: u16 = prompt_text_default("HTTP port (for redirect + ACME challenge)", "80")
        .parse().unwrap_or(80);
    let https_port: u16 = prompt_text_default("HTTPS port", "443")
        .parse().unwrap_or(443);

    println!();
    println!("  Domain : {}", domain);
    println!("  Email  : {}", if email.is_empty() { "(none)".to_string() } else { email.clone() });
    println!("  HTTP   : {}", http_port);
    println!("  HTTPS  : {}", https_port);
    println!();

    let confirm = prompt_bool("Obtain certificate now?", true);
    if !confirm {
        let cfg = TlsConfig::disabled();
        db.tls_save(&cfg)?;
        println!("Skipping — running without HTTPS.");
        return Ok(cfg);
    }

    // Run certbot standalone to get the certificate
    println!();
    println!("Running certbot... (port 80 must be free)");
    println!();

    let cert_path = run_certbot(&domain, &email, http_port, config_dir).await?;

    let cfg = TlsConfig {
        enabled:    true,
        domain:     Some(domain.clone()),
        cert:       Some(cert_path.0),
        key:        Some(cert_path.1),
        http_port,
        https_port,
    };

    db.tls_save(&cfg)?;

    println!();
    println!("✓ Certificate obtained for {}!", domain);
    println!("  Panel will serve HTTPS on port {}.", https_port);
    println!("  Certificate auto-renewal is handled by certbot's systemd timer.");
    println!("  To reconfigure, delete config/tls.json and restart the panel.");
    println!();

    Ok(cfg)
}

// ── Certbot ───────────────────────────────────────────────────────────────

fn certbot_available() -> bool {
    std::process::Command::new("certbot")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Returns (cert_path, key_path).
async fn run_certbot(
    domain:    &str,
    email:     &str,
    http_port: u16,
    config_dir: &str,
) -> Result<(String, String)> {
    // We use certbot standalone mode — it spins up its own HTTP server on port 80
    // to answer the ACME challenge. The panel is not yet listening, so port 80 is free.
    let mut args = vec![
        "certonly".to_string(),
        "--standalone".to_string(),
        "--non-interactive".to_string(),
        "--agree-tos".to_string(),
        "-d".to_string(), domain.to_string(),
        "--http-01-port".to_string(), http_port.to_string(),
    ];

    if email.is_empty() {
        args.push("--register-unsafely-without-email".to_string());
    } else {
        args.push("-m".to_string());
        args.push(email.to_string());
    }

    let status = tokio::process::Command::new("certbot")
        .args(&args)
        .status()
        .await
        .context("running certbot")?;

    if !status.success() {
        anyhow::bail!(
            "certbot failed — check the output above.\n\
             Make sure port {} is open and the domain resolves to this server.",
            http_port
        );
    }

    // Standard certbot output location
    let live = format!("/etc/letsencrypt/live/{}", domain);
    let cert = format!("{}/fullchain.pem", live);
    let key  = format!("{}/privkey.pem", live);

    if !Path::new(&cert).exists() || !Path::new(&key).exists() {
        anyhow::bail!(
            "Certificate files not found at {}\n\
             certbot may have placed them under a different name.",
            live
        );
    }

    // Set up certbot auto-renewal hook so the panel reloads certs after renewal
    setup_renewal_hook(domain, config_dir);

    Ok((cert, key))
}

/// Write a deploy hook that touches a file the panel watches for cert rotation.
fn setup_renewal_hook(domain: &str, config_dir: &str) {
    let hooks_dir = format!("/etc/letsencrypt/renewal-hooks/deploy");
    let hook_path = format!("{}/minecraft-panel.sh", hooks_dir);
    let reload_flag = PathBuf::from(config_dir).join("cert_renewed");

    let script = format!(
        "#!/bin/sh\n# Auto-generated by minecraft-panel\n\
         # Touch a file so the panel knows to reload its TLS certificate.\n\
         touch '{}'\n",
        reload_flag.display()
    );

    if let Err(e) = std::fs::create_dir_all(&hooks_dir) {
        tracing::warn!("Could not create renewal hooks dir: {}", e);
        return;
    }
    if let Err(e) = std::fs::write(&hook_path, &script) {
        tracing::warn!("Could not write renewal hook: {}", e);
        return;
    }
    // chmod +x
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755));
    }
    tracing::info!("Renewal hook written to {}", hook_path);
    let _ = domain; // suppress unused warning
}

// ── Prompt helpers ────────────────────────────────────────────────────────

fn print_banner() {
    println!();
    println!("╔══════════════════════════════════════════╗");
    println!("║     Miyoubi Panel — First-Run Setup      ║");
    println!("╚══════════════════════════════════════════╝");
    println!();
}

fn prompt_bool(question: &str, default: bool) -> bool {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        print!("  {} {} ", question, hint);
        io::stdout().flush().unwrap();
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        let trimmed = line.trim().to_lowercase();
        match trimmed.as_str() {
            "" => return default,
            "y" | "yes" => return true,
            "n" | "no"  => return false,
            _ => println!("  Please enter y or n."),
        }
    }
}

fn prompt_text(question: &str) -> String {
    print!("  {} ", question);
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    line.trim().to_string()
}

fn prompt_text_default(question: &str, default: &str) -> String {
    print!("  {} [{}]: ", question, default);
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    let trimmed = line.trim();
    if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() }
}
