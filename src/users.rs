//! User account management.
//!
//! Users are stored in `config/users.json`.
//! Two roles:
//!   - Admin  : full access (default for the first account)
//!   - Viewer : read-only — can see status cards, players, and use
//!              Start / Stop / Restart buttons. No files, mods, config, etc.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Viewer,
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserRole::Admin  => write!(f, "admin"),
            UserRole::Viewer => write!(f, "viewer"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password: String,   // stored plain-text for simplicity
    pub role:     UserRole,
    /// If Some, viewer users can only see/control these server IDs.
    /// None means unrestricted (all servers). Ignored for admin role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_servers: Option<Vec<String>>,
}

/// What we expose over the API (no password).
#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub username: String,
    pub role:     UserRole,
    pub allowed_servers: Option<Vec<String>>,
}

impl From<&User> for UserInfo {
    fn from(u: &User) -> Self {
        UserInfo {
            username: u.username.clone(),
            role: u.role.clone(),
            allowed_servers: u.allowed_servers.clone(),
        }
    }
}

// ── Store ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UserStore {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    path:  PathBuf,
    users: Vec<User>,
}

impl UserStore {
    /// Load (or create) the user store from `config_dir/users.json`.
    /// If the file doesn't exist, an admin account `admin`/`admin` is created.
    pub fn load(config_dir: &str) -> Result<Self> {
        let path = PathBuf::from(config_dir).join("users.json");

        let users: Vec<User> = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .context("reading users.json")?;
            serde_json::from_str(&raw).context("parsing users.json")?
        } else {
            // Bootstrap: create default admin account
            let defaults = vec![User {
                username: "admin".into(),
                password: "admin".into(),
                role:     UserRole::Admin,
                allowed_servers: None,
            }];
            let raw = serde_json::to_string_pretty(&defaults)?;
            std::fs::write(&path, &raw).context("writing users.json")?;
            defaults
        };

        Ok(Self {
            inner: Arc::new(RwLock::new(Inner { path, users })),
        })
    }

    fn save(inner: &Inner) -> Result<()> {
        let raw = serde_json::to_string_pretty(&inner.users)?;
        std::fs::write(&inner.path, &raw).context("saving users.json")
    }

    /// Authenticate. Returns the user's role on success.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<UserRole> {
        let inner = self.inner.read().unwrap();
        inner.users.iter().find(|u| u.username == username && u.password == password)
            .map(|u| u.role.clone())
    }

    /// List all users (without passwords).
    pub fn list(&self) -> Vec<UserInfo> {
        self.inner.read().unwrap().users.iter().map(UserInfo::from).collect()
    }

    /// Create a new user. Fails if username already exists.
    pub fn create(&self, username: &str, password: &str, role: UserRole) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        if inner.users.iter().any(|u| u.username == username) {
            anyhow::bail!("user '{}' already exists", username);
        }
        if username.is_empty() || password.is_empty() {
            anyhow::bail!("username and password must not be empty");
        }
        inner.users.push(User { username: username.into(), password: password.into(), role, allowed_servers: None });
        Self::save(&inner)
    }

    /// Delete a user. Fails if trying to delete the last admin.
    pub fn delete(&self, username: &str) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        let idx = inner.users.iter().position(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;

        // Prevent deleting the last admin
        let is_admin = inner.users[idx].role == UserRole::Admin;
        let admin_count = inner.users.iter().filter(|u| u.role == UserRole::Admin).count();
        if is_admin && admin_count == 1 {
            anyhow::bail!("cannot delete the last admin account");
        }

        inner.users.remove(idx);
        Self::save(&inner)
    }

    /// Update a user's password and/or role.
    pub fn update(&self, username: &str, password: Option<&str>, role: Option<UserRole>) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        let user = inner.users.iter_mut().find(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;

        if let Some(p) = password {
            if p.is_empty() { anyhow::bail!("password must not be empty"); }
            user.password = p.into();
        }
        if let Some(r) = role {
            user.role = r;
        }
        Self::save(&inner)
    }

    /// Look up a single user's role by username (for cookie validation).
    pub fn role_of(&self, username: &str) -> Option<UserRole> {
        self.inner.read().unwrap()
            .users.iter().find(|u| u.username == username)
            .map(|u| u.role.clone())
    }

    /// Get the list of server IDs this user can access (None = all).
    pub fn allowed_servers(&self, username: &str) -> Option<Vec<String>> {
        self.inner.read().unwrap()
            .users.iter().find(|u| u.username == username)
            .and_then(|u| u.allowed_servers.clone())
    }

    /// Set which server IDs a viewer can access. Pass None to allow all.
    pub fn set_allowed_servers(&self, username: &str, ids: Option<Vec<String>>) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        let user = inner.users.iter_mut().find(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;
        user.allowed_servers = ids;
        Self::save(&inner)
    }
}
