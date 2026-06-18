use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;
use crate::patch::diff::PatchResult;
use crate::patch::engine::PatchEngine;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ApplyPatchRequest {
    pub llm_response: String,
}

#[derive(Debug, Deserialize)]
pub struct PreviewPatchRequest {
    pub llm_response: String,
}

#[derive(Debug, Serialize)]
pub struct ApplyPatchResponse {
    pub patch_id: String,
    pub result: PatchResult,
    pub plan: String,
    pub rollback_available: bool,
}

#[derive(Debug, Serialize)]
pub struct RevertPatchResponse {
    pub patch_id: String,
    pub reverted_files: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewPatchResponse {
    pub plan: String,
    pub previews: Vec<PreviewEntry>,
    pub all_valid: bool,
}

#[derive(Debug, Serialize)]
pub struct PreviewEntry {
    pub file: String,
    pub operation: String,
    pub valid: bool,
    pub error: Option<String>,
    pub before_lines: Option<usize>,
    pub after_lines: Option<usize>,
}

pub async fn preview_patch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PreviewPatchRequest>,
) -> Result<Json<PreviewPatchResponse>, AppError> {
    let engine = PatchEngine::new(&state.project_root);
    let patch = engine.parse_llm_response(&req.llm_response)?;

    let previews = engine.preview(&patch).await;
    let all_valid = previews.iter().all(|p| p.valid);

    let entries = previews
        .into_iter()
        .map(|p| PreviewEntry {
            file: p.file,
            operation: p.operation,
            valid: p.valid,
            error: p.error,
            before_lines: p.before.as_deref().map(|s| s.lines().count()),
            after_lines: p.after.as_deref().map(|s| s.lines().count()),
        })
        .collect();

    Ok(Json(PreviewPatchResponse {
        plan: patch.plan,
        previews: entries,
        all_valid,
    }))
}

pub async fn apply_patch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ApplyPatchRequest>,
) -> Result<Json<ApplyPatchResponse>, AppError> {
    let engine = PatchEngine::new(&state.project_root);
    let patch = engine.parse_llm_response(&req.llm_response)?;
    let plan = patch.plan.clone();

    let (result, snapshot) = engine.apply_with_rollback(&patch).await;

    let patch_id = Uuid::new_v4().to_string();
    let rollback_available = snapshot.is_some();
    if let Some(snap) = snapshot {
        state.store_snapshot(&patch_id, snap);
    }

    Ok(Json(ApplyPatchResponse {
        patch_id,
        result,
        plan,
        rollback_available,
    }))
}

pub async fn revert_patch(
    State(state): State<Arc<AppState>>,
    Path(patch_id): Path<String>,
) -> Result<Json<RevertPatchResponse>, AppError> {
    let snapshot = state.take_snapshot(&patch_id).ok_or_else(|| {
        AppError::InvalidRequest(format!("No snapshot found for patch_id: {patch_id}"))
    })?;

    let outcomes = snapshot.rollback(&state.project_root).await;
    let mut reverted = Vec::new();
    let mut errors = Vec::new();
    for (file, result) in outcomes {
        match result {
            Ok(()) => reverted.push(file),
            Err(e) => errors.push(format!("{file}: {e}")),
        }
    }

    Ok(Json(RevertPatchResponse {
        patch_id,
        reverted_files: reverted,
        errors,
    }))
}
