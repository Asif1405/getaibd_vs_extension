//! Centralized vs per-project GetAIBD paths.
//!
//! - **Global** (`~/.getaibd/`): machine-local generated state (env records, memory DB,
//!   MEMORY.md, MCP defaults, sessions). Never pollutes project trees.
//! - **Project config** (`<repo>/.getaibd/`): optional, user-authored files shared with
//!   the team (`AGENTS.md`, `rules/`, `skills/`, optional `mcp.json`). We read these when
//!   present but do not create the directory for generated state.

use std::path::{Path, PathBuf};

/// `~/.getaibd` — global CLI/agent state root.
pub fn global_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".getaibd"))
}

/// Stable id for a project root (FNV-1a over canonical path).
pub fn project_slug(project_root: &Path) -> String {
    let canon = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let s = canon.to_string_lossy();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// `~/.getaibd/projects/<slug>/` — generated per-project state (memory DB, MEMORY.md, …).
pub fn project_state_dir(project_root: &Path) -> Option<PathBuf> {
    global_dir().map(|g| g.join("projects").join(project_slug(project_root)))
}

/// Ensure `~/.getaibd/projects/<slug>/` exists; returns `None` if `HOME` is unset.
pub fn ensure_project_state_dir(project_root: &Path) -> Option<PathBuf> {
    let dir = project_state_dir(project_root)?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

/// Optional per-repo authored config (`AGENTS.md`, `rules/`, `skills/`). Never auto-created.
pub fn project_config_dir(project_root: &Path) -> PathBuf {
    project_root.join(".getaibd")
}

/// Global env store — one file keyed by canonical project root path.
pub fn global_env_store_path() -> Option<PathBuf> {
    global_dir().map(|g| g.join("envs.json"))
}

/// Resolve MEMORY.md: global state dir first, then legacy per-repo locations (read-only).
pub fn resolve_memory_md_path(project_root: &Path) -> PathBuf {
    if let Some(state) = project_state_dir(project_root) {
        let global = state.join("MEMORY.md");
        if global.exists() {
            return global;
        }
    }
    let nested = project_config_dir(project_root).join("MEMORY.md");
    if nested.exists() {
        return nested;
    }
    let legacy = project_root.join("MEMORY.md");
    if legacy.exists() {
        return legacy;
    }
    // Default write target: centralized state dir (created on first write).
    project_state_dir(project_root)
        .map(|d| d.join("MEMORY.md"))
        .unwrap_or(nested)
}

/// Default memory index DB under `~/.getaibd/projects/<slug>/memory.db`.
pub fn default_memory_db_path(project_root: &Path) -> PathBuf {
    project_state_dir(project_root)
        .map(|d| d.join("memory.db"))
        .unwrap_or_else(|| project_root.join(".mcp-memory.db"))
}

/// MCP config: `~/.getaibd/mcp.json`, with optional per-repo override.
pub fn resolve_mcp_config_path(project_root: &Path) -> PathBuf {
    let project = project_config_dir(project_root).join("mcp.json");
    if project.is_file() {
        return project;
    }
    global_dir()
        .map(|g| g.join("mcp.json"))
        .unwrap_or(project)
}

/// Path for CLI/UI to create or edit the global MCP config.
pub fn global_mcp_config_path() -> Option<PathBuf> {
    global_dir().map(|g| g.join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_stable() {
        let a = project_slug(Path::new("/tmp/foo"));
        let b = project_slug(Path::new("/tmp/foo"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }
}
