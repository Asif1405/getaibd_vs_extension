use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::context::ContextConfig;
use crate::state::AppState;

#[derive(Serialize)]
pub struct ConfigView {
    server: ServerView,
    agent: AgentView,
    memory: MemoryView,
    context: ContextView,
    providers: Vec<ProviderView>,
}

#[derive(Serialize)]
struct ServerView {
    public_url: Option<String>,
    has_auth_token: bool,
}

#[derive(Serialize)]
struct AgentView {
    project_root: String,
    max_iterations: u32,
}

#[derive(Serialize)]
struct MemoryView {
    enabled: bool,
    top_k: usize,
    max_entries: usize,
}

#[derive(Serialize)]
struct ContextView {
    enabled: bool,
    max_tokens: Option<usize>,
    strategy: String,
    reserve_for_completion: f32,
}

#[derive(Serialize)]
struct ProviderView {
    id: String,
    name: String,
}

impl From<&ContextConfig> for ContextView {
    fn from(c: &ContextConfig) -> Self {
        Self {
            enabled: c.enabled,
            max_tokens: c.max_tokens,
            strategy: format!("{:?}", c.strategy),
            reserve_for_completion: c.reserve_for_completion,
        }
    }
}

#[allow(clippy::unused_async)]
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<ConfigView> {
    let providers: Vec<ProviderView> = state
        .providers
        .iter()
        .map(|(id, p)| ProviderView {
            id: id.clone(),
            name: p.display_name().to_string(),
        })
        .collect();

    Json(ConfigView {
        server: ServerView {
            public_url: state.public_url.clone(),
            has_auth_token: state.auth_token.is_some(),
        },
        agent: AgentView {
            project_root: state.project_root.display().to_string(),
            max_iterations: state.max_iterations,
        },
        memory: MemoryView {
            enabled: state.memory_store.is_some(),
            top_k: state.memory_top_k,
            max_entries: state.memory_max_entries,
        },
        context: ContextView::from(&state.context_config),
        providers,
    })
}

#[derive(Deserialize)]
pub struct ConfigUpdate {
    #[serde(default)]
    pub agent: Option<AgentUpdate>,
    #[serde(default)]
    pub context: Option<ContextUpdate>,
}

#[derive(Deserialize)]
pub struct AgentUpdate {
    pub max_iterations: Option<u32>,
}

#[derive(Deserialize)]
pub struct ContextUpdate {
    pub enabled: Option<bool>,
    pub max_tokens: Option<usize>,
    pub reserve_for_completion: Option<f32>,
}

#[derive(Serialize)]
pub struct ConfigUpdateResponse {
    ok: bool,
    message: String,
}

#[allow(clippy::unused_async)]
pub async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> Json<ConfigUpdateResponse> {
    let mut changed = Vec::new();

    if let Some(agent) = &update.agent {
        if let Some(_max) = agent.max_iterations {
            changed.push("agent.max_iterations");
        }
    }

    if let Some(ctx) = &update.context {
        if ctx.enabled.is_some() {
            changed.push("context.enabled");
        }
        if ctx.max_tokens.is_some() {
            changed.push("context.max_tokens");
        }
        if ctx.reserve_for_completion.is_some() {
            changed.push("context.reserve_for_completion");
        }
    }

    if changed.is_empty() {
        return Json(ConfigUpdateResponse {
            ok: true,
            message: "No changes applied".into(),
        });
    }

    let config_path = state.project_root.join("config.toml");
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    if let Some(agent) = &update.agent {
        if let Some(max) = agent.max_iterations {
            let tbl = doc.entry("agent").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            if let Some(tbl) = tbl.as_table_mut() {
                tbl.insert("max_iterations", toml_edit::value(i64::from(max)));
            }
        }
    }

    if let Some(ctx) = &update.context {
        let tbl = doc.entry("context").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        if let Some(tbl) = tbl.as_table_mut() {
            if let Some(enabled) = ctx.enabled {
                tbl.insert("enabled", toml_edit::value(enabled));
            }
            if let Some(max_tokens) = ctx.max_tokens {
                tbl.insert("max_tokens", toml_edit::value(max_tokens as i64));
            }
            if let Some(reserve) = ctx.reserve_for_completion {
                tbl.insert("reserve_for_completion", toml_edit::value(f64::from(reserve)));
            }
        }
    }

    match std::fs::write(&config_path, doc.to_string()) {
        Ok(()) => Json(ConfigUpdateResponse {
            ok: true,
            message: format!("Updated: {}. Restart server to apply changes.", changed.join(", ")),
        }),
        Err(e) => Json(ConfigUpdateResponse {
            ok: false,
            message: format!("Failed to write config: {e}"),
        }),
    }
}
