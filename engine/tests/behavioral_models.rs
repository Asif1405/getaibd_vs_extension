//! Behavioral E2E across EVERY catalog model, driven through the REAL agent loop
//! against the live GetAIBD gateway. Same task to each model; we record what each
//! actually did and print a comparison table.
//!
//! Ignored by default (needs a live key + spends credits). Run with:
//!   GETAIBD_API_KEY=... cargo test -p mcp-universal --test behavioral_models -- --ignored --nocapture
//! Optional filters:
//!   GETAIBD_ONLY="llama-4-maverick,kimi-k2-thinking"   # subset
//!   GETAIBD_BASE_URL=https://getaibd.com/v1/api        # override gateway
//!   GETAIBD_TIMEOUT_SECS=150                            # per-model wall clock

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcp_universal::agent::runtime::{
    run_agent_with_memory, AgentEvent, AgentEventKind, AgentOptions,
};
use mcp_universal::agent::session::Session;
use mcp_universal::agent::thinking::model_uses_reasoning;
use mcp_universal::providers::openai_compat::OpenAiCompatProvider;
use mcp_universal::providers::Provider;
use mcp_universal::tools::ToolRegistry;

const TASK: &str = "Create exactly two files in the project root.\n\
1. A file named `greeting.txt` whose entire contents are this single line:\n\
Hello from GetAIBD\n\
2. A file named `info.json` containing a JSON object with EXACTLY these keys: \
\"language\" set to the string \"rust\", and \"count\" set to the number 3.\n\
After both files are written, stop.";

struct Row {
    model: String,
    ctx_k: u64,
    iters: u32,
    tool_calls: u32,
    mutating: u32,
    used_plan: bool,
    greeting_ok: bool,
    info_ok: bool,
    completed: bool,
    secs: u64,
    note: String,
}

async fn fetch_models(base: &str, key: &str) -> Vec<(String, u64, Vec<String>)> {
    let body = reqwest::Client::new()
        .get(format!("{base}/models"))
        .bearer_auth(key)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("GET /models")
        .json::<serde_json::Value>()
        .await
        .expect("parse /models json");
    body["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap_or_default().to_string();
            let ctx = m["context_window"].as_u64().unwrap_or(0);
            let caps = m["capabilities"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (id, ctx, caps)
        })
        .collect()
}

fn verify_greeting(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join("greeting.txt"))
        .map(|s| s.trim() == "Hello from GetAIBD")
        .unwrap_or(false)
}

fn verify_info(dir: &std::path::Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(dir.join("info.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    v.get("language").and_then(|x| x.as_str()) == Some("rust")
        && v.get("count").and_then(serde_json::Value::as_i64) == Some(3)
}

async fn run_one(model: &str, ctx_k: u64, base: &str, key: &str, timeout: Duration) -> Row {
    let tmp = std::env::temp_dir().join(format!(
        "behave-{}-{}",
        model.replace(['/', '.', ':'], "_"),
        nanos()
    ));
    let _ = std::fs::create_dir_all(&tmp);
    // git init so git_status/git_diff report real changes to the reviewer.
    let _ = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&tmp)
        .status();

    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatProvider::new(
        "getaibd".to_string(),
        "GetAIBD".to_string(),
        base.to_string(),
        Some(key.to_string()),
        model.to_string(),
        180,
        2,
        true,
    ));
    let registry = ToolRegistry::build_default(&tmp);
    let mut session = Session::new("getaibd", model, tmp.clone()).with_max_iterations(25);

    let opts = AgentOptions {
        enable_thinking: model_uses_reasoning(model),
        auto_complete: true,
        ..Default::default()
    };

    let debug = std::env::var("GETAIBD_DEBUG").is_ok();
    let counters: Arc<Mutex<(u32, u32, bool, bool)>> = Arc::new(Mutex::new((0, 0, false, false)));
    let c = counters.clone();
    let mut on_event = move |e: AgentEvent| {
        if debug {
            let short: String = e.content.as_deref().unwrap_or("").chars().take(160).collect();
            eprintln!("    [{}] {}", kind_label(&e.kind), short.replace('\n', " "));
        }
        let mut g = c.lock().unwrap();
        match e.kind {
            AgentEventKind::ToolCall => {
                g.0 += 1;
                let name = serde_json::from_str::<serde_json::Value>(
                    e.content.as_deref().unwrap_or("{}"),
                )
                .ok()
                .and_then(|v| v["name"].as_str().map(str::to_string))
                .unwrap_or_default();
                if matches!(
                    name.as_str(),
                    "write_file" | "patch_file" | "move_file" | "delete_file"
                ) {
                    g.1 += 1;
                }
                if name == "update_plan" {
                    g.2 = true;
                }
            }
            AgentEventKind::Complete => g.3 = true,
            _ => {}
        }
    };

    let start = Instant::now();
    let run = run_agent_with_memory(
        &mut session,
        TASK,
        &provider,
        &registry,
        None,
        Some(&opts),
        &mut on_event,
    );
    let (iters, note) = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(r)) => (r.iterations, String::new()),
        Ok(Err(e)) => (0, format!("error: {e}")),
        Err(_) => (0, "TIMEOUT".to_string()),
    };
    let secs = start.elapsed().as_secs();
    let g = counters.lock().unwrap();
    Row {
        model: model.to_string(),
        ctx_k: ctx_k / 1000,
        iters,
        tool_calls: g.0,
        mutating: g.1,
        used_plan: g.2,
        greeting_ok: verify_greeting(&tmp),
        info_ok: verify_info(&tmp),
        completed: g.3,
        secs,
        note,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live E2E: needs GETAIBD_API_KEY and spends credits"]
async fn behavioral_all_models() {
    let Ok(key) = std::env::var("GETAIBD_API_KEY") else {
        eprintln!("SKIP: set GETAIBD_API_KEY to run the behavioral E2E");
        return;
    };
    let base = std::env::var("GETAIBD_BASE_URL")
        .unwrap_or_else(|_| "https://getaibd.com/v1/api".to_string());
    let timeout = Duration::from_secs(
        std::env::var("GETAIBD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150),
    );
    let only: Option<Vec<String>> = std::env::var("GETAIBD_ONLY")
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect());

    let models = fetch_models(&base, &key).await;
    let mut rows: Vec<Row> = Vec::new();
    let mut chat_only: Vec<String> = Vec::new();

    for (id, ctx, caps) in models {
        if let Some(filter) = &only {
            if !filter.iter().any(|f| f == &id) {
                continue;
            }
        }
        if !caps.iter().any(|c| c == "tools") {
            chat_only.push(id);
            continue;
        }
        eprintln!("→ running {id} …");
        let row = run_one(&id, ctx, &base, &key, timeout).await;
        eprintln!(
            "   {id}: iters={} tools={} mut={} plan={} greeting={} info={} done={} {}s {}",
            row.iters,
            row.tool_calls,
            row.mutating,
            row.used_plan,
            row.greeting_ok,
            row.info_ok,
            row.completed,
            row.secs,
            row.note
        );
        rows.push(row);
    }

    rows.sort_by(|a, b| a.model.cmp(&b.model));
    println!("\n## Behavioral results — same task, every tool-capable model\n");
    println!("Task: create `greeting.txt` (exact line) + `info.json` (exact keys), then stop.\n");
    println!("| Model | Ctx(k) | Iters | Tools | Mut | Plan | greeting.txt | info.json | Result | Time |");
    println!("|---|--:|--:|--:|--:|:--:|:--:|:--:|:--|--:|");
    for r in &rows {
        let result = if r.note == "TIMEOUT" {
            "⏱ timeout".to_string()
        } else if !r.note.is_empty() {
            format!("⚠ {}", r.note)
        } else if r.greeting_ok && r.info_ok {
            "✅ both files".to_string()
        } else if r.greeting_ok || r.info_ok {
            "🟡 partial".to_string()
        } else {
            "❌ no files".to_string()
        };
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {}s |",
            r.model,
            r.ctx_k,
            r.iters,
            r.tool_calls,
            r.mutating,
            if r.used_plan { "✓" } else { "·" },
            if r.greeting_ok { "✅" } else { "❌" },
            if r.info_ok { "✅" } else { "❌" },
            result,
            r.secs,
        );
    }
    if !chat_only.is_empty() {
        println!("\n_Chat-only (no tools capability, not agent-capable): {}_", chat_only.join(", "));
    }

    let ok = rows.iter().filter(|r| r.greeting_ok && r.info_ok).count();
    println!("\n**{}/{} models completed the task fully.**", ok, rows.len());
}

fn kind_label(k: &AgentEventKind) -> &'static str {
    match k {
        AgentEventKind::Start => "Start",
        AgentEventKind::Think => "Think",
        AgentEventKind::ToolCall => "ToolCall",
        AgentEventKind::ToolResult => "ToolResult",
        AgentEventKind::Response => "Response",
        AgentEventKind::Complete => "Complete",
        AgentEventKind::Error => "Error",
        AgentEventKind::ModeSelected => "ModeSelected",
        AgentEventKind::Planning => "Planning",
        AgentEventKind::Thinking => "Thinking",
        AgentEventKind::Reflecting => "Reflecting",
        AgentEventKind::Replanning => "Replanning",
        AgentEventKind::ContextCompressed => "ContextCompressed",
        AgentEventKind::FileEdit => "FileEdit",
        AgentEventKind::ApprovalRequired => "ApprovalRequired",
        AgentEventKind::TerminalExec => "TerminalExec",
        AgentEventKind::AskRequired => "AskRequired",
        AgentEventKind::StepLimitReached => "StepLimitReached",
        AgentEventKind::DiscardDraft => "DiscardDraft",
    }
}

fn nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
