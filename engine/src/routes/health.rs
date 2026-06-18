use axum::extract::State;
use axum::Json;
use std::sync::Arc;

use crate::models::HealthResponse;
use crate::state::AppState;

pub async fn health_check(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
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
