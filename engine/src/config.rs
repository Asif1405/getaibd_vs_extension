use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub context: crate::context::ContextConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_memory_db_path")]
    pub db_path: String,
    #[serde(default = "default_memory_top_k")]
    pub top_k: usize,
    #[serde(default = "default_memory_max_entries")]
    pub max_entries: usize,
    /// Ceiling on how many files a single startup full-repo index pass will embed, so the
    /// first pass on a large repo can't run away on (billed) embedding cost. 0 = no cap;
    /// the on-change watcher backfills whatever the pass leaves out.
    #[serde(default = "default_memory_max_index_files")]
    pub max_index_files: usize,
    #[serde(default = "default_memory_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_memory_chunk_overlap")]
    pub chunk_overlap: usize,
    #[serde(default = "default_memory_embedding_provider")]
    pub embedding_provider: String,
    #[serde(default)]
    pub embedding_model: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: default_memory_db_path(),
            top_k: default_memory_top_k(),
            max_entries: default_memory_max_entries(),
            max_index_files: default_memory_max_index_files(),
            chunk_size: default_memory_chunk_size(),
            chunk_overlap: default_memory_chunk_overlap(),
            embedding_provider: default_memory_embedding_provider(),
            embedding_model: None,
        }
    }
}

fn default_memory_db_path() -> String {
    ".mcp-memory.db".to_string()
}
fn default_memory_top_k() -> usize {
    5
}
fn default_memory_max_entries() -> usize {
    10_000
}
fn default_memory_max_index_files() -> usize {
    2_000
}
fn default_memory_chunk_size() -> usize {
    512
}
fn default_memory_chunk_overlap() -> usize {
    64
}
fn default_memory_embedding_provider() -> String {
    "getaibd".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_project_root")]
    pub project_root: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_command_allowlist")]
    pub command_allowlist: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            project_root: default_project_root(),
            max_iterations: default_max_iterations(),
            command_allowlist: default_command_allowlist(),
        }
    }
}

fn default_project_root() -> String {
    ".".to_string()
}
fn default_max_iterations() -> u32 {
    250
}
fn default_command_allowlist() -> Vec<String> {
    crate::tools::command::default_allowlist()
        .into_iter()
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitProviderConfig {
    #[serde(default)]
    pub requests_per_minute: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub rate_limits: HashMap<String, RateLimitProviderConfig>,
    #[serde(default)]
    pub openai_compat: Vec<OpenAiCompatConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiCompatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub default_model: String,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub supports_tool_calling: bool,
}

fn default_true() -> bool {
    true
}
fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    3333
}
fn default_timeout() -> u64 {
    // Idle/read timeout: max seconds with no bytes from the model before we treat the
    // stream as dead. Active generations reset this on every token, so a long-but-live
    // response is never cut off; only a truly stalled connection fails. Generous enough
    // to tolerate a slow first token on heavy reasoning / buffering gateways.
    300
}
fn default_retries() -> u32 {
    3
}
fn default_server() -> ServerConfig {
    ServerConfig {
        host: default_host(),
        port: default_port(),
        request_timeout_secs: default_timeout(),
        public_url: None,
        auth_token: None,
    }
}

impl AppConfig {
    pub fn load(config_path: Option<&Path>) -> Self {
        let mut cfg = if let Some(path) = config_path {
            if path.exists() {
                Self::from_toml(path)
            } else {
                tracing::warn!(
                    "Config file not found at {}, using defaults + env vars",
                    path.display()
                );
                Self::default_config()
            }
        } else {
            let default_path = Path::new("config.toml");
            if default_path.exists() {
                Self::from_toml(default_path)
            } else {
                Self::default_config()
            }
        };

        cfg.apply_env_vars();
        cfg
    }

    fn from_toml(path: &Path) -> Self {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        toml::from_str(&content).unwrap_or_else(|e| {
            tracing::error!("Failed to parse config file: {e}");
            Self::default_config()
        })
    }

    fn default_config() -> Self {
        Self {
            server: default_server(),
            providers: ProvidersConfig::default(),
            agent: AgentConfig::default(),
            memory: MemoryConfig::default(),
            context: crate::context::ContextConfig::default(),
        }
    }

    fn apply_env_vars(&mut self) {
        self.apply_server_env();
        self.apply_agent_env();
        self.apply_provider_env();
        self.apply_getaibd_env();
        self.apply_memory_env();
    }

    /// GetAIBD preset: make GetAIBD the sole provider (chat + embeddings) when
    /// `GETAIBD_API_KEY` is set, via its OpenAI-compatible API.
    fn apply_getaibd_env(&mut self) {
        let Ok(key) = std::env::var("GETAIBD_API_KEY") else {
            return;
        };
        if key.is_empty() {
            return;
        }
        let base_url = std::env::var("GETAIBD_BASE_URL")
            .unwrap_or_else(|_| "https://getaibd.com/v1/api".to_string());
        let default_model = std::env::var("GETAIBD_DEFAULT_MODEL").unwrap_or_default();

        self.providers.openai_compat = vec![OpenAiCompatConfig {
            enabled: true,
            id: "getaibd".to_string(),
            display_name: Some("GetAIBD".to_string()),
            base_url,
            api_key: Some(key),
            default_model,
            request_timeout_secs: default_timeout(),
            max_retries: default_retries(),
            supports_tool_calling: true,
        }];

        self.memory.embedding_provider = "getaibd".to_string();
        if self.memory.embedding_model.is_none() {
            self.memory.embedding_model = Some(
                std::env::var("GETAIBD_EMBEDDING_MODEL")
                    .unwrap_or_else(|_| "text-embedding-3-small".to_string()),
            );
        }
    }

    fn apply_server_env(&mut self) {
        if let Ok(host) = std::env::var("MCP_SERVER_HOST") {
            self.server.host = host;
        }
        if let Ok(port) = std::env::var("MCP_SERVER_PORT") {
            if let Ok(p) = port.parse() {
                self.server.port = p;
            }
        }
        if let Ok(url) = std::env::var("MCP_PUBLIC_URL") {
            self.server.public_url = Some(url);
        }
        if let Ok(val) = std::env::var("MCP_SERVER_REQUEST_TIMEOUT_SECS") {
            if let Ok(t) = val.parse() {
                self.server.request_timeout_secs = t;
            }
        }
        if let Ok(token) = std::env::var("MCP_AUTH_TOKEN") {
            if !token.is_empty() {
                self.server.auth_token = Some(token);
            }
        }
    }

    fn apply_agent_env(&mut self) {
        if let Ok(val) = std::env::var("MCP_AGENT_PROJECT_ROOT") {
            self.agent.project_root = val;
        }
        if let Ok(val) = std::env::var("MCP_AGENT_MAX_ITERATIONS") {
            if let Ok(n) = val.parse() {
                self.agent.max_iterations = n;
            }
        }
    }

    fn apply_provider_env(&mut self) {
        if let Ok(url) = std::env::var("OPENAI_COMPAT_BASE_URL") {
            if !url.is_empty() {
                let already_exists = self
                    .providers
                    .openai_compat
                    .iter()
                    .any(|c| c.id == "openai_compat");
                if !already_exists {
                    self.providers.openai_compat.push(OpenAiCompatConfig {
                        enabled: true,
                        id: "openai_compat".to_string(),
                        display_name: Some("OpenAI Compatible".to_string()),
                        base_url: url,
                        api_key: std::env::var("OPENAI_COMPAT_API_KEY").ok(),
                        default_model: std::env::var("OPENAI_COMPAT_DEFAULT_MODEL")
                            .unwrap_or_default(),
                        request_timeout_secs: default_timeout(),
                        max_retries: default_retries(),
                        supports_tool_calling: false,
                    });
                }
            }
        }
    }

    fn apply_memory_env(&mut self) {
        if let Ok(val) = std::env::var("MCP_MEMORY_ENABLED") {
            self.memory.enabled = val == "1" || val.eq_ignore_ascii_case("true");
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_DB_PATH") {
            self.memory.db_path = val;
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_EMBEDDING_PROVIDER") {
            self.memory.embedding_provider = val;
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_EMBEDDING_MODEL") {
            self.memory.embedding_model = Some(val);
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_TOP_K") {
            if let Ok(n) = val.parse() {
                self.memory.top_k = n;
            }
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_MAX_ENTRIES") {
            if let Ok(n) = val.parse() {
                self.memory.max_entries = n;
            }
        }
        if let Ok(val) = std::env::var("MCP_MEMORY_CHUNK_SIZE") {
            if let Ok(n) = val.parse() {
                self.memory.chunk_size = n;
            }
        }
    }
}
