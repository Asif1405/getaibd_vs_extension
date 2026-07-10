use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use futures::Stream;
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tower::ServiceExt;

use mcp_universal::error::AppError;
use mcp_universal::models::{ChatRequest, ChatResponse, ModelInfo, ProviderHealth};
use mcp_universal::providers::Provider;
use mcp_universal::routes;
use mcp_universal::state::AppState;

struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        "mock"
    }
    fn display_name(&self) -> &'static str {
        "Mock Provider"
    }
    fn max_retries(&self) -> u32 {
        0
    }
    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth {
            provider: "mock".into(),
            healthy: true,
            message: None,
        }
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        Ok(vec![ModelInfo {
            id: "mock-model".into(),
            name: "Mock Model".into(),
            capabilities: vec![],
            free: false,
            locked: false,
        }])
    }
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AppError> {
        Ok(ChatResponse {
            provider: "mock".into(),
            model: request.model.clone(),
            content: "mock response".into(),
            usage: None,
        })
    }
    fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<String, AppError>> + Send>> {
        Box::pin(async_stream::try_stream! {
            yield "mock ".to_string();
            yield "stream".to_string();
        })
    }
}

fn build_app() -> Router {
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("mock".to_string(), Arc::new(MockProvider));

    let project_root = std::env::temp_dir();
    let config = mcp_universal::config::AppConfig {
        server: mcp_universal::config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 3333,
            request_timeout_secs: 60,
            public_url: None,
            auth_token: None,
        },
        providers: mcp_universal::config::ProvidersConfig::default(),
        agent: mcp_universal::config::AgentConfig::default(),
        memory: mcp_universal::config::MemoryConfig::default(),
        context: mcp_universal::context::ContextConfig::default(),
    };
    let mut state = AppState::from_config(&config);
    state.providers = providers;
    state.project_root.clone_from(&project_root);
    state.tool_registry = mcp_universal::tools::ToolRegistry::build_default(&project_root);
    let state = Arc::new(state);

    Router::new()
        .route("/mcp/chat", post(routes::chat::chat_handler))
        .route("/sse/chat", post(routes::sse::sse_chat_handler))
        .route("/providers", get(routes::providers::list_providers))
        .route("/providers/:id/models", get(routes::providers::list_models))
        .route("/health", get(routes::health::health_check))
        .with_state(state)
}

#[tokio::test]
async fn get_providers_returns_200() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let providers: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "mock");
    assert_eq!(providers[0]["name"], "Mock Provider");
}

#[tokio::test]
async fn get_health_returns_200_with_mock() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health["status"], "healthy");
    let providers = health["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider"], "mock");
    assert!(providers[0]["healthy"].as_bool().unwrap());
}

#[tokio::test]
async fn get_models_returns_mock_models() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/providers/mock/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let models: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "mock-model");
}

#[tokio::test]
async fn get_models_unknown_provider_returns_400() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/providers/nonexistent/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response.status() == StatusCode::BAD_REQUEST || response.status() == StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn post_chat_returns_mock_response() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"mock","model":"mock-model","messages":[{"role":"user","content":"hi"}],"api_key":"test-key"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let resp: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp["provider"], "mock");
    assert_eq!(resp["content"], "mock response");
}

#[tokio::test]
async fn post_chat_empty_messages_returns_400() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"mock","model":"mock-model","messages":[]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn post_chat_unknown_provider_returns_400() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"nonexistent","model":"x","messages":[{"role":"user","content":"hi"}],"api_key":"test-key"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["error"]["code"], "UNKNOWN_PROVIDER");
}

#[tokio::test]
async fn post_chat_invalid_json_returns_bad_request() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp/chat")
                .header("content-type", "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_sse_chat_returns_stream() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sse/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"mock","model":"mock-model","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
}

#[tokio::test]
async fn post_sse_chat_empty_messages_returns_400() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sse/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"mock","model":"mock-model","messages":[]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn post_sse_chat_unknown_provider_returns_400() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sse/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"provider":"nonexistent","model":"x","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
