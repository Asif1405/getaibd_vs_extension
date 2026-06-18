//! Per-repo environment manager.
//!
//! Detects the project language from marker files (requirements.txt, pyproject.toml,
//! package.json, Cargo.toml, go.mod, Gemfile, …), creates virtual environments when
//! they don't exist, and provides the correct shell prefix/env-vars so that
//! `run_command` executes inside the right environment.
//!
//! Env metadata is persisted to `<project_root>/.mcp-envs.json` so restarts are cheap.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

use crate::error::AppError;
use crate::tools::Tool;

// ── detected project language ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectLang {
    Python,
    Node,
    Rust,
    Go,
    Ruby,
    Unknown,
}

impl ProjectLang {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Node => "node",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Ruby => "ruby",
            Self::Unknown => "unknown",
        }
    }
}

// ── per-repo env record ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvRecord {
    pub lang: ProjectLang,
    /// Absolute path to the virtualenv / node_modules / toolchain root.
    pub env_path: PathBuf,
    /// Shell prefix to prepend before commands (e.g. `source .venv/bin/activate &&`).
    pub activation_prefix: String,
    /// Extra env-vars to inject (e.g. VIRTUAL_ENV, PATH prepend).
    pub env_vars: HashMap<String, String>,
    /// Timestamp of creation.
    pub created_at: String,
}

// ── persistent store ─────────────────────────────────────────────────

const ENVS_FILE: &str = ".mcp-envs.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EnvStore {
    /// keyed by canonical project root path
    envs: HashMap<String, EnvRecord>,
}

impl EnvStore {
    fn path_for(root: &Path) -> PathBuf {
        root.join(ENVS_FILE)
    }

    fn load(root: &Path) -> Self {
        let p = Self::path_for(root);
        if let Ok(bytes) = std::fs::read(&p) {
            serde_json::from_slice(&bytes).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn save(&self, root: &Path) -> Result<(), AppError> {
        let p = Self::path_for(root);
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| AppError::InvalidRequest(format!("env store serialise: {e}")))?;
        std::fs::write(&p, bytes)
            .map_err(|e| AppError::InvalidRequest(format!("env store write: {e}")))?;
        Ok(())
    }
}

// ── public API ───────────────────────────────────────────────────────

/// Shared, thread-safe environment manager.
#[derive(Clone)]
pub struct EnvManager {
    inner: Arc<RwLock<EnvStore>>,
    root: Arc<PathBuf>,
}

impl EnvManager {
    pub fn new(root: Arc<PathBuf>) -> Self {
        let store = EnvStore::load(&root);
        Self {
            inner: Arc::new(RwLock::new(store)),
            root,
        }
    }

    // ── language detection ────────────────────────────────────────

    pub fn detect_lang(dir: &Path) -> ProjectLang {
        let markers: &[(&str, ProjectLang)] = &[
            ("requirements.txt", ProjectLang::Python),
            ("pyproject.toml", ProjectLang::Python),
            ("Pipfile", ProjectLang::Python),
            ("setup.py", ProjectLang::Python),
            ("setup.cfg", ProjectLang::Python),
            (".python-version", ProjectLang::Python),
            ("package.json", ProjectLang::Node),
            ("Cargo.toml", ProjectLang::Rust),
            ("go.mod", ProjectLang::Go),
            ("Gemfile", ProjectLang::Ruby),
        ];
        for (file, lang) in markers {
            if dir.join(file).exists() {
                return *lang;
            }
        }
        ProjectLang::Unknown
    }

    // ── env lookup / ensure ──────────────────────────────────────

    /// Get the env record for a directory (usually the project root).
    pub async fn get_env(&self, dir: &Path) -> Option<EnvRecord> {
        let key = dir.to_string_lossy().to_string();
        self.inner.read().await.envs.get(&key).cloned()
    }

    /// Ensure an env exists for `dir`.  If it doesn't, create one.
    /// Returns the record (existing or freshly-created).
    pub async fn ensure_env(&self, dir: &Path) -> Result<EnvRecord, AppError> {
        let key = dir.to_string_lossy().to_string();

        // fast-path: already tracked
        {
            let store = self.inner.read().await;
            if let Some(rec) = store.envs.get(&key) {
                // Double-check the env dir still exists on disk
                if rec.env_path.exists() {
                    return Ok(rec.clone());
                }
                // else: stale record, re-create below
            }
        }

        let lang = Self::detect_lang(dir);
        let record = match lang {
            ProjectLang::Python => self.ensure_python_env(dir).await?,
            ProjectLang::Node => self.ensure_node_env(dir).await?,
            // Rust / Go / Ruby don't need a managed venv – just record them
            other => EnvRecord {
                lang: other,
                env_path: dir.to_path_buf(),
                activation_prefix: String::new(),
                env_vars: HashMap::new(),
                created_at: now_iso(),
            },
        };

        // persist
        {
            let mut store = self.inner.write().await;
            store.envs.insert(key, record.clone());
            store.save(&self.root)?;
        }

        Ok(record)
    }

    // ── Python ───────────────────────────────────────────────────

    async fn ensure_python_env(&self, dir: &Path) -> Result<EnvRecord, AppError> {
        // Probe for existing venvs in common locations
        let candidates = [".venv", "venv", ".env", "env"];
        for name in &candidates {
            let venv = dir.join(name);
            if venv.join("bin/activate").exists() || venv.join("Scripts/activate").exists() {
                return Ok(Self::python_record(dir, &venv));
            }
        }

        // Check if we're inside a conda env already
        if std::env::var("CONDA_PREFIX").is_ok() {
            let prefix = PathBuf::from(std::env::var("CONDA_PREFIX").unwrap());
            return Ok(EnvRecord {
                lang: ProjectLang::Python,
                env_path: prefix.clone(),
                activation_prefix: format!(
                    "conda activate {} &&",
                    prefix.file_name().unwrap_or_default().to_string_lossy()
                ),
                env_vars: HashMap::new(),
                created_at: now_iso(),
            });
        }

        // Check for poetry
        if dir.join("poetry.lock").exists() {
            let out = Command::new("poetry")
                .arg("env")
                .arg("info")
                .arg("-p")
                .current_dir(dir)
                .output()
                .await;
            if let Ok(o) = out {
                if o.status.success() {
                    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    let venv = PathBuf::from(&p);
                    if venv.exists() {
                        return Ok(Self::python_record(dir, &venv));
                    }
                }
            }
        }

        // Nothing found – create `.venv` with `python3 -m venv`
        tracing::info!("Creating Python venv at {}/{}", dir.display(), ".venv");

        let python = find_python().await;
        let status = Command::new(&python)
            .args(["-m", "venv", ".venv"])
            .current_dir(dir)
            .status()
            .await
            .map_err(|e| {
                AppError::InvalidRequest(format!("Failed to spawn `{python} -m venv .venv`: {e}"))
            })?;

        if !status.success() {
            return Err(AppError::InvalidRequest(format!(
                "`{python} -m venv .venv` exited with {status}"
            )));
        }

        let venv = dir.join(".venv");

        // Auto-install deps if a requirements / pyproject file exists
        self.auto_install_python_deps(dir, &venv).await;

        Ok(Self::python_record(dir, &venv))
    }

    fn python_record(dir: &Path, venv: &Path) -> EnvRecord {
        let bin_dir = if cfg!(windows) {
            venv.join("Scripts")
        } else {
            venv.join("bin")
        };

        let activate = if cfg!(windows) {
            format!("{} &&", bin_dir.join("activate.bat").display())
        } else {
            format!("source {} &&", venv.join("bin/activate").display())
        };

        let mut env_vars = HashMap::new();
        env_vars.insert(
            "VIRTUAL_ENV".to_string(),
            venv.to_string_lossy().to_string(),
        );
        env_vars.insert(
            "PATH".to_string(),
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );

        EnvRecord {
            lang: ProjectLang::Python,
            env_path: venv.to_path_buf(),
            activation_prefix: activate,
            env_vars,
            created_at: now_iso(),
        }
        .resolve_relative(dir)
    }

    async fn auto_install_python_deps(&self, dir: &Path, venv: &Path) {
        let pip = if cfg!(windows) {
            venv.join("Scripts/pip.exe")
        } else {
            venv.join("bin/pip")
        };

        if dir.join("requirements.txt").exists() {
            tracing::info!("Auto-installing requirements.txt in venv");
            let _ = Command::new(&pip)
                .args([
                    "install",
                    "-q",
                    "-r",
                    &dir.join("requirements.txt").to_string_lossy(),
                ])
                .current_dir(dir)
                .status()
                .await;
        } else if dir.join("pyproject.toml").exists() {
            // Try `pip install -e .` for editable install
            tracing::info!("Auto-installing pyproject.toml in venv (editable)");
            let _ = Command::new(&pip)
                .args(["install", "-q", "-e", "."])
                .current_dir(dir)
                .status()
                .await;
        } else if dir.join("setup.py").exists() {
            tracing::info!("Auto-installing setup.py in venv (editable)");
            let _ = Command::new(&pip)
                .args(["install", "-q", "-e", "."])
                .current_dir(dir)
                .status()
                .await;
        }
    }

    // ── Node ─────────────────────────────────────────────────────

    async fn ensure_node_env(&self, dir: &Path) -> Result<EnvRecord, AppError> {
        let nm = dir.join("node_modules");
        if !nm.exists() {
            tracing::info!("Running npm install in {}", dir.display());
            // Prefer bun > pnpm > yarn > npm
            let (cmd, arg) = if dir.join("bun.lockb").exists() || dir.join("bunfig.toml").exists() {
                ("bun", "install")
            } else if dir.join("pnpm-lock.yaml").exists() {
                ("pnpm", "install")
            } else if dir.join("yarn.lock").exists() {
                ("yarn", "install")
            } else {
                ("npm", "install")
            };

            let status = Command::new(cmd)
                .arg(arg)
                .current_dir(dir)
                .status()
                .await
                .map_err(|e| {
                    AppError::InvalidRequest(format!("Failed to spawn `{cmd} {arg}`: {e}"))
                })?;

            if !status.success() {
                tracing::warn!("`{cmd} {arg}` exited with {status}");
            }
        }

        Ok(EnvRecord {
            lang: ProjectLang::Node,
            env_path: nm,
            activation_prefix: String::new(),
            env_vars: {
                let mut m = HashMap::new();
                let node_bin = dir.join("node_modules/.bin");
                m.insert(
                    "PATH".to_string(),
                    format!(
                        "{}:{}",
                        node_bin.display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                );
                m
            },
            created_at: now_iso(),
        })
    }

    // ── command wrapping (used by RunCommand) ────────────────────

    /// Wrap a `tokio::process::Command` with the correct env for the
    /// project at `work_dir`.  Returns the `EnvRecord` used (or None
    /// if unknown/no env needed).
    pub async fn prepare_command(&self, cmd: &mut Command, work_dir: &Path) -> Option<EnvRecord> {
        match self.ensure_env(work_dir).await {
            Ok(rec) => {
                for (k, v) in &rec.env_vars {
                    cmd.env(k, v);
                }
                Some(rec)
            }
            Err(e) => {
                tracing::warn!("env_manager: {e}");
                None
            }
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────

impl EnvRecord {
    /// Make `env_path` relative-friendly when inside the project dir.
    fn resolve_relative(mut self, _project: &Path) -> Self {
        // no-op for now — keeps absolute paths for reliability
        self.created_at = now_iso();
        self
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Find the best available Python binary.
async fn find_python() -> String {
    for candidate in ["python3", "python"] {
        let ok = Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return candidate.to_string();
        }
    }
    "python3".to_string()
}

// ── MCP tool: manage_env ─────────────────────────────────────────────
// Exposes env management to the agent so it can explicitly request
// env creation / info / reset.

pub struct ManageEnv {
    mgr: EnvManager,
}

impl ManageEnv {
    pub fn new(mgr: EnvManager) -> Self {
        Self { mgr }
    }
}

#[async_trait]
impl Tool for ManageEnv {
    fn name(&self) -> &'static str {
        "manage_env"
    }

    fn description(&self) -> &'static str {
        "Detect, create, or inspect project virtual environments (Python venvs, \
         Node node_modules, etc.). Actions: 'ensure' (detect / create), 'info' \
         (return current env record), 'reset' (delete and recreate)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["ensure", "info", "reset"],
                    "description": "Action to perform"
                },
                "path": {
                    "type": "string",
                    "description": "Subdirectory relative to project root (default: project root)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let action = input["action"]
            .as_str()
            .ok_or_else(|| AppError::InvalidRequest("action is required".into()))?;

        let sub = input["path"].as_str().unwrap_or("");
        let dir = if sub.is_empty() {
            self.mgr.root.as_ref().clone()
        } else {
            self.mgr.root.join(sub)
        };

        match action {
            "ensure" => {
                let rec = self.mgr.ensure_env(&dir).await?;
                Ok(json!({
                    "status": "ok",
                    "lang": rec.lang.as_str(),
                    "env_path": rec.env_path.to_string_lossy(),
                    "activation_prefix": rec.activation_prefix,
                }))
            }
            "info" => {
                if let Some(rec) = self.mgr.get_env(&dir).await {
                    Ok(json!({
                        "status": "ok",
                        "lang": rec.lang.as_str(),
                        "env_path": rec.env_path.to_string_lossy(),
                        "activation_prefix": rec.activation_prefix,
                        "created_at": rec.created_at,
                    }))
                } else {
                    let lang = EnvManager::detect_lang(&dir);
                    Ok(json!({
                        "status": "no_env",
                        "detected_lang": lang.as_str(),
                        "hint": "Run with action='ensure' to create an environment.",
                    }))
                }
            }
            "reset" => {
                // Remove existing env record + directory, then re-ensure
                if let Some(rec) = self.mgr.get_env(&dir).await {
                    if rec.env_path.exists()
                        && rec.env_path != dir
                        && (rec.lang == ProjectLang::Python || rec.lang == ProjectLang::Node)
                    {
                        let _ = tokio::fs::remove_dir_all(&rec.env_path).await;
                        tracing::info!("Removed env at {}", rec.env_path.display());
                    }
                    // Remove from store
                    let key = dir.to_string_lossy().to_string();
                    let mut store = self.mgr.inner.write().await;
                    store.envs.remove(&key);
                    let _ = store.save(&self.mgr.root);
                }
                let rec = self.mgr.ensure_env(&dir).await?;
                Ok(json!({
                    "status": "reset_ok",
                    "lang": rec.lang.as_str(),
                    "env_path": rec.env_path.to_string_lossy(),
                }))
            }
            other => Err(AppError::InvalidRequest(format!("Unknown action: {other}"))),
        }
    }
}
