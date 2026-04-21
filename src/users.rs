//! User account management — backed by SQLite via `crate::db::Db`.
//!
//! Two roles:
//!   - Admin  : full access
//!   - Viewer : read-only status, players, Start/Stop/Restart only
//!
//! On first run the `users` table is empty. `needs_setup()` returns true
//! and the panel serves the /setup wizard. Once the first admin is created,
//! setup is permanently disabled.

use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::db::Db;

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

impl FromStr for UserRole {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        match s {
            "admin"  => Ok(UserRole::Admin),
            "viewer" => Ok(UserRole::Viewer),
            _        => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username:        String,
    pub password:        String,
    pub role:            UserRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_servers: Option<Vec<String>>,
}

/// What we expose over the API (no password).
#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub username:        String,
    pub role:            UserRole,
    pub allowed_servers: Option<Vec<String>>,
}

impl From<&User> for UserInfo {
    fn from(u: &User) -> Self {
        UserInfo {
            username:        u.username.clone(),
            role:            u.role.clone(),
            allowed_servers: u.allowed_servers.clone(),
        }
    }
}

// ── Store ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UserStore {
    db: Db,
}

impl UserStore {
    /// Create a UserStore backed by the shared database.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// True when no users exist yet (first run — setup wizard required).
    pub fn needs_setup(db: &Db) -> bool {
        db.user_count().unwrap_or(0) == 0
    }

    /// Authenticate. Returns the user's role on success.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<UserRole> {
        self.db.users_all().ok()?
            .into_iter()
            .find(|u| u.username == username && u.password == password)
            .map(|u| u.role)
    }

    /// True if no users exist.
    pub fn is_empty(&self) -> bool {
        self.db.user_count().unwrap_or(0) == 0
    }

    /// List all users (without passwords).
    pub fn list(&self) -> Vec<UserInfo> {
        self.db.users_all()
            .unwrap_or_default()
            .iter()
            .map(UserInfo::from)
            .collect()
    }

    /// Create a new user. Fails if username already exists.
    pub fn create(&self, username: &str, password: &str, role: UserRole) -> Result<()> {
        if username.is_empty() || password.is_empty() {
            anyhow::bail!("username and password must not be empty");
        }
        let all = self.db.users_all()?;
        if all.iter().any(|u| u.username == username) {
            anyhow::bail!("user '{}' already exists", username);
        }
        self.db.user_upsert(&User {
            username:        username.into(),
            password:        password.into(),
            role,
            allowed_servers: None,
        })
    }

    /// Delete a user. Fails if trying to delete the last admin.
    pub fn delete(&self, username: &str) -> Result<()> {
        let all = self.db.users_all()?;
        let target = all.iter().find(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;
        if target.role == UserRole::Admin && self.db.admin_count()? <= 1 {
            anyhow::bail!("cannot delete the last admin account");
        }
        self.db.user_delete(username)
    }

    /// Update a user's password and/or role.
    pub fn update(&self, username: &str, password: Option<&str>, role: Option<UserRole>) -> Result<()> {
        let mut all = self.db.users_all()?;
        let user = all.iter_mut().find(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;
        if let Some(p) = password {
            if p.is_empty() { anyhow::bail!("password must not be empty"); }
            user.password = p.into();
        }
        if let Some(r) = role {
            user.role = r;
        }
        self.db.user_upsert(user)
    }

    /// Look up a single user's role (for cookie validation).
    pub fn role_of(&self, username: &str) -> Option<UserRole> {
        self.db.users_all().ok()?
            .into_iter()
            .find(|u| u.username == username)
            .map(|u| u.role)
    }

    /// Get the list of server IDs this user can access (None = all).
    pub fn allowed_servers(&self, username: &str) -> Option<Vec<String>> {
        self.db.users_all().ok()?
            .into_iter()
            .find(|u| u.username == username)
            .and_then(|u| u.allowed_servers)
    }

    /// Set which server IDs a viewer can access. Pass None to allow all.
    pub fn set_allowed_servers(&self, username: &str, ids: Option<Vec<String>>) -> Result<()> {
        let mut all = self.db.users_all()?;
        let user = all.iter_mut().find(|u| u.username == username)
            .ok_or_else(|| anyhow::anyhow!("user not found"))?;
        user.allowed_servers = ids;
        self.db.user_upsert(user)
    }

    /// Create the initial admin account (called by /api/setup).
    pub fn bootstrap(&self, username: &str, password: &str) -> Result<()> {
        self.create(username, password, UserRole::Admin)
            .context("creating initial admin account")
    }
}
