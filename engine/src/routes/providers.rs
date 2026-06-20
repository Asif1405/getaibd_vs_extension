use axum::extract::{Path, State};
use axum::Json;
use std::sync::Arc;

use crate::error::AppError;
use crate::models::{ModelInfo, ProviderInfo};
use crate::state::AppState;

#[allow(clippy::unused_async)]
pub async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Vec<ProviderInfo>> {
    let mut providers: Vec<ProviderInfo> = state
        .providers
        .values()
        .map(|p| ProviderInfo {
            id: p.id().to_string(),
            name: p.display_name().to_string(),
        })
        .collect();
    providers.sort_by(|a, b| a.id.cmp(&b.id));
    Json(providers)
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> Result<Json<Vec<ModelInfo>>, AppError> {
    let provider = state
        .get_provider(&provider_id)
        .ok_or(AppError::UnknownProvider(provider_id))?;

    let models = provider.list_models().await?;
    Ok(Json(models))
}
