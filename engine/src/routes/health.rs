use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::models::HealthResponse;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct HealthQuery {
    /// When truthy (`?providers=1`), also probe upstream provider connectivity.
    #[serde(default)]
    providers: Option<String>,
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1" | "true" | "yes" | "on"))
}

/// Liveness/health probe.
///
/// Bare `GET /health` is a CHEAP, purely local liveness check — it makes no
/// upstream calls and returns immediately. This matters: the extension's process
/// supervisor uses `/health` as a liveness probe, and when this handler did a
/// (potentially slow) round of provider connectivity checks, a sluggish upstream
/// could make the probe time out and the supervisor would SIGTERM a perfectly
/// healthy engine *mid-run*. Provider connectivity is now opt-in via `?providers=1`.
pub async fn health_check(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HealthQuery>,
) -> Json<HealthResponse> {
    if !truthy(query.providers.as_deref()) {
        return Json(HealthResponse {
            status: "healthy".to_string(),
            providers: Vec::new(),
        });
    }

    let mut provider_checks = Vec::new();
    for provider in state.providers.values() {
        provider_checks.push(provider.health_check().await);
    }
    provider_checks.sort_by(|a, b| a.provider.cmp(&b.provider));

    let all_healthy = provider_checks.iter().all(|p| p.healthy);

    Json(HealthResponse {
        status: if all_healthy {
            "healthy".to_string()
        } else {
            "degraded".to_string()
        },
        providers: provider_checks,
    })
}

/// Codebase index freshness/status: whether the semantic index is enabled, how
/// many chunks are stored, and the last index activity (startup pass + watcher).
pub async fn index_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let enabled = state.memory_store.is_some();
    let total_chunks = state
        .memory_store
        .as_ref()
        .and_then(|s| s.count().ok())
        .unwrap_or(0);

    let mut body = state.index_status.snapshot();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("enabled".to_string(), json!(enabled));
        obj.insert("total_chunks".to_string(), json!(total_chunks));
    }
    Json(body)
}
