use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use crate::agent::task_queue::TaskQueue;
use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::config::AppConfig;
use crate::memory::cache::ProjectAnalysisCache;
use crate::memory::embeddings::OpenAiEmbedder;
use crate::memory::{EmbeddingProvider, MemoryStore};
use crate::patch::engine::Snapshot;
use crate::providers::openai_compat::OpenAiCompatProvider;
use crate::providers::Provider;
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::tools::approval::ApprovalGate;
use crate::tools::ToolRegistry;

pub struct AppState {
    pub providers: HashMap<String, Arc<dyn Provider>>,
    pub public_url: Option<String>,
    pub auth_token: Option<String>,
    pub tool_registry: ToolRegistry,
    pub project_root: PathBuf,
    pub max_iterations: u32,
    pub memory_store: Option<MemoryStore>,
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// GetAIBD API key + base URL (from the `getaibd` provider), so session tools like
    /// `web_search` can call the platform's developer API with the user's own key/billing.
    pub getaibd_api_key: Option<String>,
    pub getaibd_base_url: Option<String>,
    pub memory_top_k: usize,
    pub memory_max_entries: usize,
    /// Resolved path of the memory DB (used to site the sibling merkle index).
    pub memory_db_path: Option<PathBuf>,
    /// Ceiling for the startup full-repo index pass.
    pub memory_max_index_files: usize,
    pub rate_limiter: Arc<RateLimiter>,
    pub circuit_breaker: Arc<CircuitBreaker>,
    pub task_queue: Arc<TaskQueue>,
    /// Shared project analysis cache (call graph, project graph, types, recent tracker)
    pub analysis_cache: Arc<ProjectAnalysisCache>,
    /// Session-scoped approval gates keyed by session UUID
    approval_gates: Mutex<HashMap<String, ApprovalGate>>,
    /// Session-scoped terminal gates keyed by session UUID
    terminal_gates: Mutex<HashMap<String, crate::tools::terminal_gate::TerminalGate>>,
    /// Session-scoped ask gates (clarifying questions) keyed by session UUID
    ask_gates: Mutex<HashMap<String, crate::tools::ask_gate::AskGate>>,
    /// Snapshots for patch rollback, keyed by patch ID (UUID)
    pub patch_snapshots: Mutex<HashMap<String, Snapshot>>,
    pub context_config: crate::context::ContextConfig,
}

impl AppState {
    pub fn from_config(config: &AppConfig) -> Self {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();

        Self::register_openai_compat(&mut providers, config);

        tracing::info!(
            "Provider registry: {} provider(s) enabled: [{}]",
            providers.len(),
            providers.keys().cloned().collect::<Vec<_>>().join(", ")
        );

        let project_root = std::fs::canonicalize(&config.agent.project_root)
            .unwrap_or_else(|_| PathBuf::from(&config.agent.project_root));
        let tool_registry = ToolRegistry::build_default(&project_root);

        tracing::info!("Agent project root: {}", project_root.display());

        let (memory_store, embedder) = if config.memory.enabled {
            let db_path = project_root.join(&config.memory.db_path);
            if let Some(e) = Self::build_embedder(config) {
                let dim = e.dimension();
                match MemoryStore::open(&db_path, dim) {
                    Ok(store) => {
                        tracing::info!(
                            "Memory enabled: {} (dim={dim}), db={}",
                            config.memory.embedding_provider,
                            db_path.display()
                        );
                        (Some(store), Some(e))
                    }
                    Err(err) => {
                        tracing::warn!("Memory store failed to open: {err}");
                        (None, None)
                    }
                }
            } else {
                tracing::info!("Memory disabled: no embedding provider configured");
                (None, None)
            }
        } else {
            (None, None)
        };

        let rl_limits = config
            .providers
            .rate_limits
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    RateLimitConfig {
                        requests_per_minute: v.requests_per_minute,
                    },
                )
            })
            .collect();
        let rate_limiter = RateLimiter::new(rl_limits);

        let circuit_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default()));
        let task_queue = Arc::new(TaskQueue::new());
        let analysis_cache = Arc::new(ProjectAnalysisCache::new());

        let memory_db_path = memory_store
            .as_ref()
            .map(|_| project_root.join(&config.memory.db_path));
        let memory_max_index_files = config.memory.max_index_files;

        let getaibd_cfg = config
            .providers
            .openai_compat
            .iter()
            .find(|c| c.id == "getaibd");
        let getaibd_api_key = getaibd_cfg
            .and_then(|c| c.api_key.clone())
            .filter(|k| !k.is_empty());
        let getaibd_base_url = getaibd_cfg.map(|c| c.base_url.clone());

        Self {
            providers,
            public_url: config.server.public_url.clone(),
            auth_token: config.server.auth_token.clone(),
            tool_registry,
            project_root,
            max_iterations: config.agent.max_iterations,
            memory_store,
            embedder,
            getaibd_api_key,
            getaibd_base_url,
            memory_top_k: config.memory.top_k,
            memory_max_entries: config.memory.max_entries,
            memory_db_path,
            memory_max_index_files,
            rate_limiter,
            circuit_breaker,
            task_queue,
            analysis_cache,
            approval_gates: Mutex::new(HashMap::new()),
            terminal_gates: Mutex::new(HashMap::new()),
            ask_gates: Mutex::new(HashMap::new()),
            patch_snapshots: Mutex::new(HashMap::new()),
            context_config: config.context.clone(),
        }
    }

    /// Register an approval gate for a specific agent session.
    pub fn set_approval_gate(&self, session_id: &str, gate: ApprovalGate) {
        self.approval_gates
            .lock().expect("state mutex poisoned")
            .insert(session_id.to_string(), gate);
    }

    /// Retrieve the approval gate for a session (clones it so callers hold their own handle).
    pub fn get_approval_gate(&self, session_id: &str) -> Option<ApprovalGate> {
        self.approval_gates.lock().expect("state mutex poisoned").get(session_id).cloned()
    }

    /// Remove the approval gate after a session finishes.
    pub fn clear_approval_gate(&self, session_id: &str) {
        self.approval_gates.lock().expect("state mutex poisoned").remove(session_id);
    }

    /// Find any active gate — used as fallback when session_id is unknown.
    pub fn any_approval_gate(&self) -> Option<ApprovalGate> {
        self.approval_gates.lock().expect("state mutex poisoned").values().next().cloned()
    }

    /// Register a terminal gate for a specific agent session.
    pub fn set_terminal_gate(
        &self,
        session_id: &str,
        gate: crate::tools::terminal_gate::TerminalGate,
    ) {
        self.terminal_gates
            .lock().expect("state mutex poisoned")
            .insert(session_id.to_string(), gate);
    }

    /// Retrieve the terminal gate for a session.
    pub fn get_terminal_gate(
        &self,
        session_id: &str,
    ) -> Option<crate::tools::terminal_gate::TerminalGate> {
        self.terminal_gates.lock().expect("state mutex poisoned").get(session_id).cloned()
    }

    /// Remove the terminal gate after a session finishes.
    pub fn clear_terminal_gate(&self, session_id: &str) {
        self.terminal_gates.lock().expect("state mutex poisoned").remove(session_id);
    }

    /// Find any active terminal gate — fallback when session_id is unknown.
    pub fn any_terminal_gate(&self) -> Option<crate::tools::terminal_gate::TerminalGate> {
        self.terminal_gates.lock().expect("state mutex poisoned").values().next().cloned()
    }

    /// Register an ask gate (clarifying questions) for a specific agent session.
    pub fn set_ask_gate(&self, session_id: &str, gate: crate::tools::ask_gate::AskGate) {
        self.ask_gates
            .lock().expect("state mutex poisoned")
            .insert(session_id.to_string(), gate);
    }

    /// Retrieve the ask gate for a session.
    pub fn get_ask_gate(&self, session_id: &str) -> Option<crate::tools::ask_gate::AskGate> {
        self.ask_gates.lock().expect("state mutex poisoned").get(session_id).cloned()
    }

    /// Remove the ask gate after a session finishes.
    pub fn clear_ask_gate(&self, session_id: &str) {
        self.ask_gates.lock().expect("state mutex poisoned").remove(session_id);
    }

    /// Find any active ask gate — fallback when session_id is unknown.
    pub fn any_ask_gate(&self) -> Option<crate::tools::ask_gate::AskGate> {
        self.ask_gates.lock().expect("state mutex poisoned").values().next().cloned()
    }

    /// Store a patch snapshot for later rollback.
    pub fn store_snapshot(&self, patch_id: &str, snapshot: Snapshot) {
        self.patch_snapshots
            .lock().expect("state mutex poisoned")
            .insert(patch_id.to_string(), snapshot);
    }

    /// Take (consume) a snapshot, removing it from the store.
    pub fn take_snapshot(&self, patch_id: &str) -> Option<Snapshot> {
        self.patch_snapshots.lock().expect("state mutex poisoned").remove(patch_id)
    }

    fn build_embedder(config: &AppConfig) -> Option<Arc<dyn EmbeddingProvider>> {
        let provider = &config.memory.embedding_provider;
        match provider.as_str() {
            "getaibd" => {
                let cfg = config
                    .providers
                    .openai_compat
                    .iter()
                    .find(|c| c.id == "getaibd")?;
                let key = cfg.api_key.clone().unwrap_or_default();
                if key.is_empty() {
                    return None;
                }
                let model = config
                    .memory
                    .embedding_model
                    .clone()
                    .unwrap_or_else(|| "text-embedding-3-small".to_string());
                let embedder = OpenAiEmbedder::new(key)
                    .with_base_url(cfg.base_url.clone())
                    .with_model(model);
                Some(Arc::new(embedder))
            }
            _ => {
                tracing::warn!("Unknown embedding provider: {provider}");
                None
            }
        }
    }

    fn register_openai_compat(
        providers: &mut HashMap<String, Arc<dyn Provider>>,
        config: &AppConfig,
    ) {
        for cfg in &config.providers.openai_compat {
            if !cfg.enabled || cfg.base_url.is_empty() {
                continue;
            }
            let label = cfg.display_name.clone().unwrap_or_else(|| cfg.id.clone());
            tracing::info!(
                "Registering provider: {} ({}) -> {}",
                cfg.id,
                label,
                cfg.base_url
            );
            providers.insert(
                cfg.id.clone(),
                Arc::new(OpenAiCompatProvider::new(
                    cfg.id.clone(),
                    label,
                    cfg.base_url.clone(),
                    cfg.api_key.clone(),
                    cfg.default_model.clone(),
                    cfg.request_timeout_secs,
                    cfg.max_retries,
                    cfg.supports_tool_calling,
                )),
            );
        }
    }

    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }
}
