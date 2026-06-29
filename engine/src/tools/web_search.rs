use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use crate::error::AppError;
use crate::tools::Tool;

/// Web search backed by the GetAIBD developer API (`POST {base_url}/web_search`),
/// which proxies Tavily and bills the user's own org. Lets the agent look up current
/// package versions, library docs and error messages online instead of reading a
/// project's vendored dependency trees (`.venv`, `node_modules`, `site-packages`).
pub struct WebSearch {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web for up-to-date information: latest package/library versions, API \
         and usage docs, changelogs, release notes, and error messages. Use this INSTEAD of \
         reading a project's dependency directories (.venv, node_modules, vendor, \
         site-packages, target) to learn what a library does or which version is current. \
         Returns a short synthesized answer plus the top results (title, url, snippet). \
         Each call costs a small number of credits."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for, as a natural-language query" },
                "max_results": { "type": "integer", "description": "Number of results to return (1-10, default 5)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> Result<Value, AppError> {
        let query = input["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err(AppError::InvalidRequest("query is required".into()));
        }
        let max_results = input["max_results"].as_u64().unwrap_or(5).clamp(1, 10);

        let url = format!("{}/web_search", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&json!({ "query": query, "max_results": max_results }))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| AppError::InvalidRequest(format!("web_search request failed: {e}")))?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| AppError::InvalidRequest(format!("web_search returned a bad response: {e}")))?;

        if !status.is_success() {
            // The developer API returns errors as `{"detail": "..."}`.
            let detail = body
                .get("detail")
                .map(|d| {
                    d.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| d.to_string())
                })
                .unwrap_or_else(|| status.to_string());
            return Err(AppError::InvalidRequest(format!(
                "web_search failed ({status}): {detail}"
            )));
        }

        Ok(body)
    }
}
