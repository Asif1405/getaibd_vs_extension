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

    let app = Router::new()
        .merge(protected)
        .route("/health", get(routes::health::health_check))
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
}

fn start_file_watcher(state: &AppState) -> Option<notify::RecommendedWatcher> {
    let store = state.memory_store.clone()?;
    let embedder = state.embedder.clone()?;

    mcp_universal::memory::watcher::spawn_watcher(&state.project_root, store, embedder)
}
