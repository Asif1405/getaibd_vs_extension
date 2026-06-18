use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::agent::planner::{StepKind, TaskPlanner};
use crate::agent::task_queue::{TaskHandle, TaskRecord};
use crate::error::AppError;
use crate::patch::engine::PatchEngine;
use crate::state::AppState;
use crate::tools::ToolRegistry;

#[derive(Debug, Deserialize)]
pub struct EnqueueRequest {
    pub task: String,
    pub provider: String,
    pub context: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EnqueueResponse {
    pub task_id: String,
}

#[derive(Debug, Serialize)]
pub struct TaskListResponse {
    pub tasks: Vec<TaskRecord>,
}

pub async fn enqueue_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnqueueRequest>,
) -> Result<Json<EnqueueResponse>, AppError> {
    let provider = state
        .providers
        .get(&req.provider)
        .ok_or_else(|| AppError::InvalidRequest(format!("Unknown provider: {}", req.provider)))?
        .clone();

    let project_root = state.project_root.clone();
    let task_desc = req.task.clone();
    let context = req.context.unwrap_or_default();

    let task_id = state
        .task_queue
        .enqueue(task_desc.clone(), move |handle: TaskHandle| async move {
            handle.progress("Planning task...").await;

            let planner = TaskPlanner::new(provider.clone());
            let plan = match planner.plan(&task_desc, &context).await {
                Ok(p) => p,
                Err(e) => {
                    handle.fail(format!("Planning failed: {}", e)).await;
                    return;
                }
            };

            handle
                .progress(format!("Plan ready: {} steps", plan.steps.len()))
                .await;

            let engine = PatchEngine::new(&project_root);
            let registry = ToolRegistry::build_default(&project_root);
            let mut completed: Vec<usize> = vec![];
            let mut all_ok = true;

            loop {
                let ready = plan.next_steps_from_completed(&completed);
                if ready.is_empty() {
                    break;
                }
                let steps: Vec<_> = ready.into_iter().cloned().collect();

                for step in steps {
                    handle
                        .progress(format!(
                            "[{}] {:?}: {}",
                            step.id, step.kind, step.description
                        ))
                        .await;

                    let result: Result<String, String> = match &step.kind {
                        StepKind::RetrieveContext => {
                            let file = step.estimated_files.first().cloned().unwrap_or_default();
                            let tool = registry
                                .get("read_file")
                                .or_else(|| registry.get("search_files"));
                            match tool {
                                Some(t) => {
                                    let input = serde_json::json!({ "path": file });
                                    t.execute(input)
                                        .await
                                        .map(|v| v.to_string())
                                        .map_err(|e| e.to_string())
                                }
                                None => Ok("No file tool available".into()),
                            }
                        }
                        StepKind::GenerateCode => {
                            Ok(format!("Code generation step logged: {}", step.description))
                        }
                        StepKind::ApplyPatch => {
                            match engine.parse_llm_response(&step.description) {
                                Ok(patch) => {
                                    let (res, _snap) = engine.apply_with_rollback(&patch).await;
                                    if res.success {
                                        Ok(format!("Applied {} file(s)", res.applied.len()))
                                    } else {
                                        let errs: Vec<String> = res
                                            .failed
                                            .iter()
                                            .filter_map(|f| f.error.clone())
                                            .collect();
                                        Err(format!("Patch failed: {}", errs.join("; ")))
                                    }
                                }
                                Err(e) => Err(format!("Could not parse patch: {e}")),
                            }
                        }
                        StepKind::RunCommand => match registry.get("run_command") {
                            Some(t) => {
                                let input = serde_json::json!({ "command": step.description });
                                t.execute(input)
                                    .await
                                    .map(|v| v.to_string())
                                    .map_err(|e| e.to_string())
                            }
                            None => Err("run_command tool not available".into()),
                        },
                        StepKind::VerifyTests => match registry.get("run_command") {
                            Some(t) => {
                                let cmd = plan
                                    .verify_commands
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| "cargo test".to_string());
                                let input = serde_json::json!({ "command": cmd });
                                t.execute(input)
                                    .await
                                    .map(|v| v.to_string())
                                    .map_err(|e| e.to_string())
                            }
                            None => Err("run_command tool not available".into()),
                        },
                        StepKind::AskUser => {
                            Ok(format!("User input required: {}", step.description))
                        }
                        StepKind::Custom(_) => {
                            Ok(format!("Custom step skipped: {}", step.description))
                        }
                    };

                    match result {
                        Ok(output) => {
                            handle
                                .progress(format!("  ✓ step {}: {}", step.id, output))
                                .await;
                            completed.push(step.id);
                        }
                        Err(e) => {
                            handle.fail(format!("Step {} failed: {}", step.id, e)).await;
                            all_ok = false;
                            break;
                        }
                    }
                }

                if !all_ok {
                    break;
                }
            }

            if all_ok {
                handle
                    .complete(format!(
                        "Plan '{}' completed — {}/{} steps executed",
                        plan.task,
                        completed.len(),
                        plan.steps.len()
                    ))
                    .await;
            }
        })
        .await;

    Ok(Json(EnqueueResponse { task_id }))
}

pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskRecord>, AppError> {
    state
        .task_queue
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| AppError::InvalidRequest(format!("Task not found: {}", id)))
}

pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TaskListResponse>, AppError> {
    let tasks = state.task_queue.list().await;
    Ok(Json(TaskListResponse { tasks }))
}

pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let cancelled = state.task_queue.cancel(&id).await;
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}
