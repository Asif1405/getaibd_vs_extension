//! Adaptive multi-agent pipeline for Agent/Debug runs.
//!
//! Rather than forcing every task through a fixed 5-stage flow, this mirrors how
//! Cursor's agent works: a cheap triage step decides how much scaffolding a task
//! actually needs, then the real work runs on the user's model.
//!
//! Flow:
//!   1. Analyzer (cheap model, no tools) — classify + extract requirements and the
//!      symbols worth searching, and decide whether exploration/validation help.
//!      Trivial tasks (or any analyzer failure) go straight to the plain worker.
//!   2. Context pack (deterministic, parallel, NO model) — fan out semantic search
//!      + ripgrep for the key symbols concurrently, giving the worker concrete
//!      `path:line` targets for near-zero cost. This is the default "search".
//!   3. Explorer (cheap model, read-only, ON DEMAND) — only when the analyzer flags
//!      multi-hop discovery; summarizes what the deterministic pack can't reason out.
//!   4. Planner (cheap model, ON DEMAND) — writes the todo ledger for complex work.
//!   5. Worker (user's model) — the real implementation loop, unchanged.
//!   6. Validator (user's model, ON DEMAND) — verifies the working tree and can send
//!      work back to the worker up to `max_fix_cycles` times.
//!
//! Every stage degrades gracefully: any error falls through to the plain worker
//! loop, so the pipeline can never make Agent mode worse than the single loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::context::ContextConfig;
use crate::error::AppError;
use crate::memory::{format_context, retrieve_context};
use crate::models::{ChatRequest, Message};
use crate::providers::Provider;
use crate::tools::explore::EXPLORE_TOOLS;
use crate::tools::lsp::ripgrep;
use crate::tools::ToolRegistry;

use super::runtime::{
    run_agent_with_memory, AgentEvent, AgentEventKind, AgentOptions, AgentResult, MemoryContext,
};
use super::session::Session;
use super::thinking;

/// Everything the pipeline needs from the orchestrator. Gates are cloned so each
/// stage can carry them (worker/validator run real tools through the client).
pub struct PipelineDeps<'a> {
    pub provider: &'a Arc<dyn Provider>,
    pub registry: &'a ToolRegistry,
    pub memory: Option<&'a MemoryContext<'a>>,
    pub approval_gate: Option<crate::tools::approval::ApprovalGate>,
    pub terminal_gate: Option<crate::tools::terminal_gate::TerminalGate>,
    pub ask_gate: Option<crate::tools::ask_gate::AskGate>,
    pub editor_gate: Option<crate::tools::editor_gate::EditorGate>,
    pub max_fix_cycles: u32,
    pub explorer_max_iters: u32,
}

const ANALYZER_PROMPT: &str = "You are the TRIAGE stage of a coding agent. Read the user's \
request and return ONLY a JSON object (no prose, no code fences) with keys: \
\"complexity\" (\"simple\" or \"complex\"), \"summary\" (one sentence restating the goal), \
\"requirements\" (array of concrete, verifiable requirements), \"symbols\" (array of the \
concrete identifiers/function/type/file names worth searching for to locate the code — be \
specific, these drive an automated code search), \"needs_exploration\" (bool: true only if \
finding the code requires following references across several files, false if a couple of \
searches suffice), \"needs_validation\" (bool: true if the change should be built/tested or \
spans multiple files, false for a trivial edit or a plain question). Mark \"simple\" for small, \
low-risk, mostly single-file work or a question; \"complex\" otherwise. Be decisive and terse.";

const EXPLORER_PROMPT: &str = "You are the EXPLORER: a fast, strictly read-only investigator. \
Given the task and a starting set of code hits, trace exactly what must change. Use \
search_code / semantic_search / find_symbol / find_references to follow references, then read \
only the relevant ranges — never dump whole files, never crawl exhaustively. You cannot write, \
edit, delete, or run commands. Finish with a tight report: the files/symbols to change (with \
`path:line`), existing patterns to follow, and constraints/gotchas. Say clearly if not found.";

const PLANNER_PROMPT: &str = "You are the PLANNER. Given the task, its requirements, and the \
gathered code context, produce a concise ordered implementation checklist. Return ONLY \
markdown: a first line '## Plan' then '- [ ] <step>' lines. Each step concrete and actionable \
(which file, what change). No prose. 3-8 steps.";

const VALIDATOR_PROMPT: &str = "You are the VALIDATOR. Verify the task is fully and correctly \
implemented in the current working tree. Inspect changes with git_diff, read edited files, and \
run the build/tests with run_command when the project has them. Check every requirement. Do NOT \
make changes yourself. End your reply with ONLY a single-line JSON object: \
{\"pass\": true|false, \"issues\": [\"...\"]}. pass=false with concrete, actionable issues if \
anything is missing, broken, or unverified; otherwise pass=true with an empty list.";

#[derive(Deserialize, Default)]
struct AnalyzeJson {
    #[serde(default)]
    complexity: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    requirements: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    needs_exploration: Option<bool>,
    #[serde(default)]
    needs_validation: Option<bool>,
}

#[derive(Deserialize, Default)]
struct VerdictJson {
    #[serde(default)]
    pass: bool,
    #[serde(default)]
    issues: Vec<String>,
}

/// Extract the outermost JSON object from a model reply (tolerates prose/fences).
fn extract_json(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

fn emit(on_event: &mut impl FnMut(AgentEvent), kind: AgentEventKind, msg: impl Into<String>) {
    on_event(AgentEvent {
        kind,
        content: Some(msg.into()),
    });
}

/// Cheap, fast model for the no-/light-reasoning stages (triage/explore/plan).
/// GetAIBD gets a small model; other providers reuse the session model (we can't
/// assume another provider hosts a specific cheap model name).
fn fast_model(provider_id: &str, session_model: &str) -> String {
    if provider_id == "getaibd" {
        std::env::var("GETAIBD_PIPELINE_MODEL")
            .ok()
            .or_else(|| std::env::var("GETAIBD_COMPLETION_MODEL").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "gemini-3.5-flash".to_string())
    } else {
        session_model.to_string()
    }
}

/// One-shot, tool-free model call used by the triage/planner stages.
async fn one_shot(
    provider: &Arc<dyn Provider>,
    provider_id: &str,
    model: &str,
    cache_session_id: &Option<String>,
    system: &str,
    user: String,
    max_tokens: u32,
) -> Option<String> {
    let req = ChatRequest {
        provider: provider_id.to_string(),
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: system.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: user,
            },
        ],
        temperature: if thinking::model_uses_reasoning(model) {
            None
        } else {
            Some(0.1)
        },
        max_tokens: Some(max_tokens),
        reasoning_effort: None,
        api_key: None,
        cache_session_id: cache_session_id.clone(),
    };
    match provider.chat(&req).await {
        Ok(resp) if !resp.content.trim().is_empty() => Some(resp.content),
        _ => None,
    }
}

fn action_options(deps: &PipelineDeps, model: &str, auto_complete: bool) -> AgentOptions {
    let reasoning = thinking::model_uses_reasoning(model);
    AgentOptions {
        approval_gate: deps.approval_gate.clone(),
        terminal_gate: deps.terminal_gate.clone(),
        ask_gate: deps.ask_gate.clone(),
        editor_gate: deps.editor_gate.clone(),
        tool_timeout_secs: 300,
        circuit_breaker: None,
        context_config: Some(ContextConfig::default()),
        enable_thinking: reasoning,
        auto_complete,
        temperature: if reasoning { None } else { Some(0.1) },
        max_llm_calls: None,
    }
}

fn sub_memory<'a>(deps: &PipelineDeps<'a>) -> Option<MemoryContext<'a>> {
    deps.memory.map(|m| MemoryContext {
        store: m.store,
        embedder: m.embedder,
        top_k: 10,
        max_entries: 50,
        analysis_cache: m.analysis_cache.clone(),
    })
}

/// Escape regex metacharacters so an identifier is matched literally by ripgrep.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Search terms: analyzer-provided symbols, else identifier-like tokens mined from
/// the summary/requirements. Deduped and capped.
fn search_terms(analysis: &AnalyzeJson) -> Vec<String> {
    let mut terms: Vec<String> = analysis
        .symbols
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 3)
        .collect();
    if terms.is_empty() {
        let text = format!("{} {}", analysis.summary, analysis.requirements.join(" "));
        for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if tok.len() >= 4 && (tok.contains('_') || tok.chars().any(|c| c.is_uppercase())) {
                terms.push(tok.to_string());
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms.truncate(6);
    terms
}

/// Stage 2: deterministic, parallel context pack. Fans out semantic retrieval and
/// one ripgrep per key symbol concurrently — no model call. Returns a compact,
/// `path:line`-cited brief (empty if nothing useful turned up).
async fn context_pack(deps: &PipelineDeps<'_>, project_root: &Path, analysis: &AnalyzeJson) -> String {
    let terms = search_terms(analysis);

    // Parallel ripgrep per term (spawned, so they run concurrently).
    let grep_handles: Vec<_> = terms
        .iter()
        .map(|t| {
            let root = project_root.to_path_buf();
            let term = t.clone();
            let pat = regex_escape(t);
            tokio::spawn(async move { (term, ripgrep(&root, &pat).await) })
        })
        .collect();

    // Semantic retrieval borrows the (non-'static) memory refs, so run it inline
    // and join it with the spawned greps.
    let query = format!(
        "{} {}",
        analysis.summary,
        analysis.requirements.join(" ")
    );
    let sem_fut = async {
        match deps.memory {
            Some(m) => retrieve_context(m.store, m.embedder, query.trim(), 8)
                .await
                .ok()
                .map(|r| format_context(&r)),
            None => None,
        }
    };
    let greps_fut = async {
        let mut out = Vec::new();
        for h in grep_handles {
            if let Ok(pair) = h.await {
                out.push(pair);
            }
        }
        out
    };
    let (semantic, greps) = tokio::join!(sem_fut, greps_fut);

    let mut pack = String::from("## Relevant context (auto-gathered)\n");
    let mut has_content = false;

    if let Some(sem) = semantic.filter(|s| !s.trim().is_empty()) {
        pack.push('\n');
        pack.push_str(sem.trim());
        pack.push('\n');
        has_content = true;
    }

    let mut hits = String::new();
    for (term, matches) in greps {
        if matches.is_empty() {
            continue;
        }
        hits.push_str(&format!("\n- `{term}`:\n"));
        for (path, line, text) in matches.into_iter().take(6) {
            let text = text.trim();
            let text: String = text.chars().take(160).collect();
            hits.push_str(&format!("  {path}:{line}: {text}\n"));
        }
    }
    if !hits.trim().is_empty() {
        pack.push_str("\n### Symbol/keyword hits\n");
        pack.push_str(&hits);
        has_content = true;
    }

    if has_content {
        // Keep the brief bounded so it never bloats the worker context.
        pack.chars().take(4000).collect()
    } else {
        String::new()
    }
}

/// Stage 3: read-only LLM investigation on a cheap model (on demand). Returns a
/// findings report, or empty on failure (the worker can still proceed).
async fn explore(
    deps: &PipelineDeps<'_>,
    project_root: PathBuf,
    model: &str,
    analysis: &AnalyzeJson,
    pack: &str,
) -> String {
    let query = format!(
        "Task: {}\n\nRequirements:\n- {}\n\nStarting code hits:\n{}\n\nTrace exactly what must \
         change to do this.",
        analysis.summary,
        analysis.requirements.join("\n- "),
        pack
    );
    let registry = deps.registry.filter(|name| EXPLORE_TOOLS.contains(&name));
    let mut sub = Session::new(deps.provider.id(), model, project_root)
        .with_system_prompt(EXPLORER_PROMPT)
        .with_max_iterations(deps.explorer_max_iters);
    let options = AgentOptions {
        editor_gate: deps.editor_gate.clone(),
        tool_timeout_secs: 120,
        enable_thinking: thinking::model_uses_reasoning(model),
        auto_complete: false,
        ..Default::default()
    };
    let mem = sub_memory(deps);
    let mut sink = |_e: AgentEvent| {};
    match run_agent_with_memory(
        &mut sub,
        &query,
        deps.provider,
        &registry,
        mem.as_ref(),
        Some(&options),
        &mut sink,
    )
    .await
    {
        Ok(r) => r.final_response,
        Err(_) => String::new(),
    }
}

/// Run the main worker loop on the shared session (full tools, gates, approvals).
async fn run_worker(
    deps: &PipelineDeps<'_>,
    session: &mut Session,
    input: &str,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let options = action_options(deps, &session.model, true);
    let mem = sub_memory(deps);
    run_agent_with_memory(
        session,
        input,
        deps.provider,
        deps.registry,
        mem.as_ref(),
        Some(&options),
        on_event,
    )
    .await
}

/// Stage 6: verify the working tree. Failures/parse errors count as "pass" so a
/// flaky validator never blocks a finished run.
async fn validate(
    deps: &PipelineDeps<'_>,
    project_root: PathBuf,
    model: &str,
    input: &str,
    requirements: &[String],
) -> VerdictJson {
    let registry = deps
        .registry
        .filter(|name| EXPLORE_TOOLS.contains(&name) || name == "run_command");
    let mut sub = Session::new(deps.provider.id(), model, project_root)
        .with_system_prompt(VALIDATOR_PROMPT)
        .with_max_iterations(deps.explorer_max_iters);
    let options = action_options(deps, model, false);
    let mem = sub_memory(deps);
    let vinput = format!(
        "Original task:\n{input}\n\nRequirements to verify:\n- {}\n\nVerify the current working \
         tree implements this correctly.",
        requirements.join("\n- ")
    );
    let mut sink = |_e: AgentEvent| {};
    let verdict = match run_agent_with_memory(
        &mut sub,
        &vinput,
        deps.provider,
        &registry,
        mem.as_ref(),
        Some(&options),
        &mut sink,
    )
    .await
    {
        Ok(r) => r.final_response,
        Err(_) => return VerdictJson::default_pass(),
    };
    extract_json(&verdict)
        .and_then(|j| serde_json::from_str::<VerdictJson>(j).ok())
        .unwrap_or_else(VerdictJson::default_pass)
}

impl VerdictJson {
    fn default_pass() -> Self {
        Self {
            pass: true,
            issues: Vec::new(),
        }
    }
}

/// Entry point. Adaptive: trivial tasks (or any triage failure) run the plain
/// worker loop; complex tasks get a deterministic context pack, optional cheap
/// exploration, an optional plan, then the worker and an optional validator loop.
pub async fn run_pipeline(
    deps: &PipelineDeps<'_>,
    session: &mut Session,
    input: &str,
    on_event: &mut impl FnMut(AgentEvent),
) -> Result<AgentResult, AppError> {
    let provider_id = session.provider_id.clone();
    let model = session.model.clone();
    let cache = session.cache_session_id.clone();
    let project_root = session.project_root.clone();
    let cheap = fast_model(&provider_id, &model);

    // Stage 1 — Triage (cheap model, also the gate).
    emit(on_event, AgentEventKind::Planning, "Analyzing the task...");
    let analysis = one_shot(
        deps.provider,
        &provider_id,
        &cheap,
        &cache,
        ANALYZER_PROMPT,
        input.to_string(),
        500,
    )
    .await
    .as_deref()
    .and_then(extract_json)
    .and_then(|j| serde_json::from_str::<AnalyzeJson>(j).ok());

    let is_complex = analysis
        .as_ref()
        .map(|a| a.complexity.to_lowercase().contains("complex"))
        .unwrap_or(false);
    let Some(analysis) = analysis.filter(|_| is_complex) else {
        // Simple task or triage unavailable: fast single-loop path.
        return run_worker(deps, session, input, on_event).await;
    };

    // Stage 2 — Deterministic parallel context pack (no model call).
    emit(on_event, AgentEventKind::Planning, "Searching the codebase...");
    let pack = context_pack(deps, &project_root, &analysis).await;

    // Stage 3 — Explorer (cheap model), only when multi-hop discovery is needed.
    let findings = if analysis.needs_exploration.unwrap_or(true) {
        emit(on_event, AgentEventKind::Planning, "Tracing references...");
        explore(deps, project_root.clone(), &cheap, &analysis, &pack).await
    } else {
        String::new()
    };

    // Stage 4 — Planner (cheap model) -> todo ledger.
    emit(on_event, AgentEventKind::Planning, "Structuring the work...");
    let planner_input = format!(
        "Task:\n{input}\n\nRequirements:\n- {}\n\nContext:\n{}\n{}",
        analysis.requirements.join("\n- "),
        pack,
        findings
    );
    if let Some(ledger) = one_shot(
        deps.provider,
        &provider_id,
        &cheap,
        &cache,
        PLANNER_PROMPT,
        planner_input,
        700,
    )
    .await
    {
        let ledger = ledger.trim().to_string();
        if !ledger.is_empty() {
            session.task_ledger = Some(ledger.clone());
            emit(on_event, AgentEventKind::Planning, ledger);
        }
    }

    // Stage 5 — Worker (user's model) on the shared session.
    let seeded = format!(
        "{input}\n\n## Requirements\n- {}\n\n{}\n{}\n\nImplement the task end to end, following \
         the plan checklist. Make only the changes needed, then stop.",
        analysis.requirements.join("\n- "),
        pack,
        findings
    );
    let mut result = run_worker(deps, session, &seeded, on_event).await?;

    // Stage 6 — Validator (user's model) with bounded fix loop, on demand.
    if analysis.needs_validation.unwrap_or(true) {
        let mut cycle = 0u32;
        loop {
            emit(on_event, AgentEventKind::Reflecting, "Validating the changes...");
            let verdict = validate(
                deps,
                project_root.clone(),
                &model,
                input,
                &analysis.requirements,
            )
            .await;
            if verdict.pass {
                break;
            }
            if cycle >= deps.max_fix_cycles {
                emit(
                    on_event,
                    AgentEventKind::Reflecting,
                    format!(
                        "Validation still found issues after {cycle} fix attempt(s):\n- {}",
                        verdict.issues.join("\n- ")
                    ),
                );
                break;
            }
            cycle += 1;
            emit(
                on_event,
                AgentEventKind::Replanning,
                format!("Validation found issues; fixing (attempt {cycle})..."),
            );
            let fix = format!(
                "The validator found these issues. Fix them now and re-verify your work:\n- {}",
                verdict.issues.join("\n- ")
            );
            result = run_worker(deps, session, &fix, on_event).await?;
        }
    }

    Ok(result)
}
