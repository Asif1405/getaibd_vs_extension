use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use axum::Router;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::EnvFilter;

use mcp_universal::config::AppConfig;
use mcp_universal::routes;
use mcp_universal::state::AppState;

#[derive(Parser)]
#[command(
    name = "mcp-universal",
    about = "Universal MCP server for GetAIBD"
)]
struct Cli {
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[arg(long)]
    host: Option<String>,

    #[arg(short, long)]
    port: Option<u16>,

    #[arg(long, env = "MCP_PUBLIC_URL")]
    public_url: Option<String>,

    #[arg(long)]
    stdio: bool,

    #[arg(long)]
    project_root: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    spawn_parent_watchdog();

    let mut app_config = AppConfig::load(cli.config.as_deref());

    if let Some(host) = cli.host {
        app_config.server.host = host;
    }
    if let Some(port) = cli.port {
        app_config.server.port = port;
    }
    if let Some(url) = cli.public_url {
        app_config.server.public_url = Some(url);
    }
    if let Some(root) = cli.project_root {
        app_config.agent.project_root = root.to_string_lossy().to_string();
    }

    if cli.stdio {
        let registry = mcp_universal::tools::ToolRegistry::build_default(
            &std::fs::canonicalize(&app_config.agent.project_root)
                .unwrap_or_else(|_| PathBuf::from(&app_config.agent.project_root)),
        );
        let handler = mcp_universal::mcp::handler::McpHandler::new(registry);
        if let Err(e) = mcp_universal::mcp::stdio::run_stdio(handler).await {
            eprintln!("Stdio error: {e}");
        }
        return;
    }

    let state = Arc::new(AppState::from_config(&app_config));

    let _watcher = start_file_watcher(&state);
    let index_state = state.clone();

    if let Some(ref url) = state.public_url {
        tracing::info!("Public URL set to {url} — SSE clients should connect here");
    }

    if state.auth_token.is_some() {
        tracing::info!(
            "Bearer token auth enabled — all endpoints except /health require Authorization header"
        );
    }

    let cors = CorsLayer::new().allow_methods(Any).allow_headers(Any);

    let protected = Router::new()
        .route("/mcp/chat", post(routes::chat::chat_handler))
        .route("/sse/chat", post(routes::sse::sse_chat_handler))
        .route("/agent/run", post(routes::agent::agent_handler))
        .route("/agent/approve", post(routes::agent::approve_handler))
        .route(
            "/agent/terminal_result",
            post(routes::agent::terminal_result_handler),
        )
        .route(
            "/agent/ask_result",
            post(routes::agent::ask_result_handler),
        )
        .route(
            "/agent/editor_result",
            post(routes::agent::editor_result_handler),
        )
        .route(
            "/agent/orchestrated",
            post(routes::orchestrated_agent::orchestrated_agent_handler),
        )
        .route("/mcp/message", post(routes::mcp_http::mcp_message_handler))
        .route("/mcp/stream", post(routes::mcp_stream::mcp_stream_handler))
        .route("/providers", get(routes::providers::list_providers))
        .route("/providers/:id/models", get(routes::providers::list_models))
        .route("/patch/preview", post(routes::patch::preview_patch))
        .route("/patch/apply", post(routes::patch::apply_patch))
        .route("/patch/revert/:id", post(routes::patch::revert_patch))
        .route("/tasks", get(routes::tasks::list_tasks))
        .route("/tasks", post(routes::tasks::enqueue_task))
        .route("/tasks/:id", get(routes::tasks::get_task))
        .route("/tasks/:id/cancel", post(routes::tasks::cancel_task))
        .route("/config", get(routes::config::get_config))
        .route("/config", put(routes::config::put_config))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            routes::auth::require_auth,
        ));

    // Image attachments are sent inline as base64 data URLs, so the request body can
    // be many MB (base64 inflates the raw bytes by ~33%). Axum's default 2 MB limit
    // rejects these with HTTP 413, so raise it to comfortably fit several images.
    const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

    let app = Router::new()
        .merge(protected)
        .route("/health", get(routes::health::health_check))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", app_config.server.host, app_config.server.port)
        .parse()
        .expect("Invalid bind address");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("MCP Universal server listening on http://{addr}");

    tokio::spawn(async move {
        run_startup_indexing(index_state.as_ref()).await;
    });

    axum::serve(listener, app).await.unwrap();
}

/// Exit when the parent that spawned us goes away — including when it is
/// SIGKILL'd, in which case no graceful shutdown ever reaches this process.
/// Without this the agent orphans (reparented to pid 1) and keeps its RAG index
/// resident in memory forever. Gated on `GETAIBD_PARENT_WATCH` so standalone or
/// manually-launched servers are never affected.
#[cfg(unix)]
fn spawn_parent_watchdog() {
    if std::env::var_os("GETAIBD_PARENT_WATCH").is_none() {
        return;
    }
    // `getppid` has no preconditions and cannot fail.
    let initial = unsafe { libc::getppid() };
    // Already orphaned (or no real parent) — nothing meaningful to watch.
    if initial <= 1 {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        // When the parent dies we are reparented, so getppid() no longer matches.
        if unsafe { libc::getppid() } != initial {
            tracing::info!(
                "parent process {initial} exited; shutting down getaibd-agent"
            );
            std::process::exit(0);
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_watchdog() {}

async fn run_startup_indexing(state: &AppState) {
    let Some(ref store) = state.memory_store else {
        return;
    };
    let Some(ref embedder) = state.embedder else {
        return;
    };

    match mcp_universal::memory::summarizer::summarize_and_index(
        &state.project_root,
        store,
        embedder.as_ref(),
    )
    .await
    {
        Ok(summary) => {
            tracing::info!(
                "Startup indexing complete: {} files, {} lines, {} languages",
                summary.total_files,
                summary.total_lines,
                summary.languages.len()
            );
        }
        Err(e) => {
            tracing::warn!("Startup indexing failed: {e}");
        }
    }

    // Full-repo incremental index for complete semantic coverage (the summary above only
    // embeds the top "important" files). The merkle hash diff makes every later startup
    // cheap — only changed files re-embed — and prunes deleted files. Bounded by
    // `memory_max_index_files` so the first pass on a large repo can't run away on
    // (billed) embedding cost; the watcher backfills the remainder as files change.
    let Some(ref db_path) = state.memory_db_path else {
        return;
    };
    let merkle_path = db_path.with_extension("merkle.db");
    match mcp_universal::memory::merkle::MerkleIndex::open(&merkle_path) {
        Ok(merkle) => {
            match mcp_universal::memory::merkle::incremental_index(
                &state.project_root,
                &merkle,
                store,
                embedder.as_ref(),
                state.memory_max_index_files,
            )
            .await
            {
                Ok(r) => tracing::info!(
                    "Full index: {} embedded, {} unchanged, {} removed (cap {})",
                    r.indexed,
                    r.unchanged,
                    r.removed,
                    state.memory_max_index_files
                ),
                Err(e) => tracing::warn!("Full index failed: {e}"),
            }
        }
        Err(e) => tracing::warn!("Merkle index open failed: {e}"),
    }
}

fn start_file_watcher(state: &AppState) -> Option<notify::RecommendedWatcher> {
    let store = state.memory_store.clone()?;
    let embedder = state.embedder.clone()?;

    mcp_universal::memory::watcher::spawn_watcher(&state.project_root, store, embedder)
}
