use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

// ── Types ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Serialize)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Serialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub binary: bool,
}

// ── Text file detection ───────────────────────────────────────────────────

const TEXT_EXTS: &[&str] = &[
    "properties", "json", "yml", "yaml", "txt", "conf", "cfg",
    "toml", "ini", "xml", "sh", "md", "env", "log", "csv",
];

fn is_text_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.rsplit('.').next().map(|ext| TEXT_EXTS.contains(&ext)).unwrap_or(false)
}

// ── File operations ───────────────────────────────────────────────────────

pub fn list_dir(path: &str) -> Result<DirListing> {
    let rd = fs::read_dir(path)
        .with_context(|| format!("reading directory {:?}", path))?;

    let mut entries = Vec::new();
    for entry in rd {
        let entry = entry.context("reading dir entry")?;
        let meta = entry.metadata().context("reading metadata")?;
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        });
    }

    Ok(DirListing { path: path.to_string(), entries })
}

pub fn read_file(path: &str) -> Result<FileContent> {
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if !is_text_file(&name) {
        return Ok(FileContent { path: path.to_string(), content: String::new(), binary: true });
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("reading file {:?}", path))?;
    Ok(FileContent { path: path.to_string(), content, binary: false })
}

pub fn write_file(path: &str, content: &str) -> Result<()> {
    fs::write(path, content).with_context(|| format!("writing file {:?}", path))
}

// ── Path traversal protection ─────────────────────────────────────────────

pub fn safe_path(data_path: &str, requested: &str) -> Result<String> {
    let base = canonicalize_or_clean(data_path);
    let req = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        base.join(requested)
    };
    let clean = normalize_path(&req);
    if !clean.starts_with(&base) {
        anyhow::bail!("path outside data directory");
    }
    Ok(clean.to_string_lossy().to_string())
}

/// Translate a frontend `/data/...` virtual path to the real host path.
pub fn map_data_path(p: &str, data_path: &str) -> String {
    if p == "/data" {
        data_path.to_string()
    } else if let Some(rest) = p.strip_prefix("/data/") {
        format!("{}/{}", data_path, rest)
    } else {
        p.to_string()
    }
}

fn canonicalize_or_clean(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    p.canonicalize().unwrap_or_else(|_| normalize_path(&p))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out.iter().collect()
}

// ── Mod management ────────────────────────────────────────────────────────

pub fn disable_mod(mod_path: &str, data_path: &str) -> Result<()> {
    let disabled_dir = format!("{}/mods/disabled", data_path);
    fs::create_dir_all(&disabled_dir).context("creating disabled directory")?;
    let file_name = Path::new(mod_path)
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid mod path"))?
        .to_string_lossy()
        .to_string();
    let dest = format!("{}/{}", disabled_dir, file_name);
    fs::rename(mod_path, &dest).context("moving mod to disabled")
}
