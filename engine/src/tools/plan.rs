use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::tools::Tool;

/// Turn a title into a filesystem-safe slug for the temp plan file name.
fn slugify(s: &str) -> String {
    let slug: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').replace("--", "-");
    let slug: String = slug.chars().take(48).collect();
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug
    }
}

/// Records the agent's task ledger (goal + step checklist). The runtime captures
/// the submitted plan onto the session and re-injects it verbatim every turn, so
/// the agent keeps its plan and place across long runs and context summarization.
pub struct UpdatePlan;

impl UpdatePlan {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for UpdatePlan {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &'static str {
        "update_plan"
    }

    fn description(&self) -> &'static str {
        "Record or update your task plan/ledger: the goal and a checklist of steps with status \
         markers ([ ] todo, [~] in progress, [x] done). Call this FIRST on any multi-step task to \
         lay out the plan, then call it again whenever you finish a step or the plan changes. The \
         latest plan is always kept visible in your context, so use it to track progress and never \
         lose your place."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full, updated plan as a markdown checklist: a one-line goal \
                                    followed by steps, each marked [ ], [~], or [x]."
                }
            },
            "required": ["plan"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let plan = input.get("plan").and_then(Value::as_str).unwrap_or("").trim();
        if plan.is_empty() {
            return Err(AppError::InvalidRequest(
                "update_plan requires a non-empty 'plan'".into(),
            ));
        }
        Ok(json!({
            "ok": true,
            "note": "Plan recorded. It stays visible in your context — keep marking steps [x] as you finish them."
        }))
    }
}

/// Explicit "the task is done" signal. The model calls this ONLY when every part of
/// the user's request is fully implemented and verified. The runtime treats the call
/// as a completion claim: it verifies the work against the actual changes and then
/// ends the run — turning "done" into a positive, unambiguous action instead of the
/// mere absence of a tool call (which is indistinguishable from a narration pause).
pub struct AttemptCompletion;

impl AttemptCompletion {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for AttemptCompletion {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AttemptCompletion {
    fn name(&self) -> &'static str {
        "attempt_completion"
    }

    fn description(&self) -> &'static str {
        "Signal that the user's task is FULLY complete. Call this ONLY when every requirement \
         has been implemented and verified — never to announce a step you are about to take. \
         Provide a `summary` of what you changed and accomplished. The runtime verifies the work \
         against the actual repository changes before ending the run; if anything is still \
         missing, keep using the other tools instead of calling this."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "A concise summary of everything you changed/accomplished for \
                                    the task — the final message shown to the user."
                }
            },
            "required": ["summary"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let summary = input
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if summary.is_empty() {
            return Err(AppError::InvalidRequest(
                "attempt_completion requires a non-empty 'summary'".into(),
            ));
        }
        Ok(json!({
            "ok": true,
            "note": "Completion recorded — the runtime will verify the work against the actual changes."
        }))
    }
}

/// Saves the final plan as a markdown file in a temp directory (outside the repo).
/// Plan mode is read-only on the project, so the plan is persisted here instead of
/// writing into the user's codebase.
pub struct WritePlan;

impl WritePlan {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for WritePlan {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WritePlan {
    fn name(&self) -> &'static str {
        "write_plan"
    }

    fn description(&self) -> &'static str {
        "Save the finished plan as a markdown file in a temporary directory OUTSIDE the \
         repository (Plan mode never modifies project files). Provide a short `title` \
         and the full markdown `plan`; returns the temp file path to share with the user. \
         To REVISE a plan you already saved this session, pass its `path` (the value \
         returned earlier) and the file is updated in place instead of creating a new one."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short plan title, used to name the temp file."
                },
                "plan": {
                    "type": "string",
                    "description": "The full plan as markdown (context, gap analysis, todos, risks, approach)."
                },
                "path": {
                    "type": "string",
                    "description": "Optional: an existing plan file path returned by a previous \
                                    write_plan call. When given, that file is overwritten in place \
                                    (so revisions edit the same plan instead of piling up new files)."
                }
            },
            "required": ["plan"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let plan = input.get("plan").and_then(Value::as_str).unwrap_or("").trim();
        if plan.is_empty() {
            return Err(AppError::InvalidRequest(
                "write_plan requires a non-empty 'plan'".into(),
            ));
        }
        let title = input.get("title").and_then(Value::as_str).unwrap_or("plan");
        let dir = std::env::temp_dir().join("getaibd-plans");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot create plan dir: {e}")))?;

        // Reuse the caller-supplied path when it points at an existing plan under our
        // temp dir (revision-in-place); otherwise mint a fresh timestamped file. The
        // containment check keeps Plan mode from writing anywhere but the plans dir.
        let reuse = input
            .get("path")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .filter(|p| p.starts_with(&dir) && p.extension().is_some_and(|e| e == "md"));
        let (path, updated) = match reuse {
            Some(p) => (p, true),
            None => {
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                (dir.join(format!("{stamp}-{}.md", slugify(title))), false)
            }
        };
        tokio::fs::write(&path, plan)
            .await
            .map_err(|e| AppError::InvalidRequest(format!("Cannot write plan: {e}")))?;
        Ok(json!({
            "ok": true,
            "path": path.to_string_lossy(),
            "updated": updated,
            "note": if updated {
                "Existing plan updated in place — the repository was not modified."
            } else {
                "Plan saved to a temp file — the repository was not modified."
            }
        }))
    }
}
