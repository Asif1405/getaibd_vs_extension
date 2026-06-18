# MCP Universal — Plan of Action

**Version 0.3** · February 2026

A Rust-based universal MCP server with SSE streaming that bridges any MCP
client (VSCode, Claude Desktop, Claude Code, Codex, and more) to any LLM
backend — local or cloud.

---

## 1. Vision

MCP Universal is the fastest, most portable bridge between developer tooling
and any LLM backend — local or cloud. A single Rust binary, zero runtime
dependencies, first-class streaming, and a polished developer experience.

### Core Principles

| Principle | What it means in practice |
|---|---|
| **Single binary** | `cargo build --release` → one executable, no Node/Python on the server |
| **Any provider** | Ollama, OpenAI, Gemini, Claude, HuggingFace, Replicate, RunPod, or any OpenAI-compatible API |
| **Any MCP client** | VSCode extension, Claude Desktop, Claude Code, Codex, or any MCP-compatible tool |
| **Stream-first** | SSE / Streamable HTTP is the default path; non-streaming is a convenience fallback |
| **Multi-transport** | HTTP + SSE for network clients, stdio for local MCP clients — no domain required |
| **Zero-config start** | `cargo run` with env vars works out of the box; TOML is optional layering |
| **Open & free** | MIT license, community-driven, no telemetry by default |

### Supported Providers (outbound — server calls them)

| Provider | Type | Auth | Status |
|---|---|---|---|
| Ollama (local) | Local | None | Phase 1 |
| Ollama (remote) | Self-hosted | None | Phase 1 |
| OpenAI | Cloud API | `OPENAI_API_KEY` | Phase 1 |
| Google Gemini | Cloud API | `GEMINI_API_KEY` | Phase 1 |
| Anthropic Claude | Cloud API | `ANTHROPIC_API_KEY` | Phase 1 |
| OpenRouter | Cloud API (multi-model gateway) | `OPENROUTER_API_KEY` | Phase 1 |
| HuggingFace Inference | Cloud API | `HUGGINGFACE_API_KEY` | Phase 3 |
| Replicate | Cloud API | `REPLICATE_API_TOKEN` | Phase 3 |
| RunPod | Cloud API / Serverless | `RUNPOD_API_KEY` | Phase 3 |
| OpenAI-compatible (LM Studio, vLLM, Together, Groq, etc.) | Any | Varies | Phase 3 |

All providers are **outbound** HTTP calls. The server is the client.
**No domain is ever needed for provider connections.**

### Supported MCP Clients (inbound — they connect to the server)

| Client | Transport | Domain needed? |
|---|---|---|
| VSCode extension (bundled) | HTTP + SSE to `localhost` | **No** |
| Claude Desktop | Stdio (launches binary directly) | **No** |
| Claude Code | Stdio | **No** |
| OpenAI Codex | Stdio | **No** |
| Any local MCP client | Stdio or `localhost` HTTP | **No** |
| Any remote MCP client | Streamable HTTP | **Yes** — needs reachable URL |

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────┐
│                   VSCode Extension                  │
│  ┌───────────┐ ┌──────────┐ ┌────────────────────┐ │
│  │ Chat Panel│ │ Commands │ │ Provider/Model     │ │
│  │ (Webview) │ │ & Menus  │ │ Picker             │ │
│  └─────┬─────┘ └────┬─────┘ └────────┬───────────┘ │
│        └─────────────┼────────────────┘             │
│                      │ HTTP / SSE                   │
└──────────────────────┼──────────────────────────────┘
                       │
          ┌────────────▼────────────┐
          │      Axum HTTP Server   │
          │  ┌────────────────────┐ │
          │  │   Router / CORS    │ │
          │  │   Middleware Stack │ │
          │  └────────┬───────────┘ │
          │           │             │
          │  ┌────────▼───────────┐ │
          │  │  Provider Registry │ │
          │  │  (Arc<dyn Provider>)│ │
          │  └────────┬───────────┘ │
          │           │             │
          │  ┌────────▼───────────┐ │
          │  │  Shared AppState   │ │
          │  │  config + providers│ │
          │  └────────────────────┘ │
          └─────────────────────────┘
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   ┌─────────┐   ┌──────────┐   ┌─────────┐
   │ Ollama  │   │ OpenAI   │   │ Claude  │
   │ local / │   │ Gemini   │   │         │
   │ remote  │   │          │   │         │
   └─────────┘   └──────────┘   └─────────┘
```

### Server Layers

1. **HTTP layer** — Axum 0.7 router, tower CORS middleware, structured
   `tracing` logging. All state is held in a shared `AppState` behind `Arc`.
2. **Provider registry** — A `HashMap<String, Arc<dyn Provider>>` built at
   startup from config. Each provider is Send + Sync.
3. **Provider trait** — Defines `chat()` (returns full response) and
   `chat_stream()` (returns `Stream<Item = Result<String>>`).
4. **Config** — Layered: compiled defaults → `config.toml` → environment
   variables. Every field is optional; env vars always win.

### Extension Layers

1. **Webview chat panel** — HTML/CSS/JS rendered in a VSCode webview. Posts
   messages to the extension host via `vscode.postMessage`.
2. **Extension host** — TypeScript process that owns the HTTP connection to the
   MCP server, manages state, and bridges the webview ↔ server.
3. **Commands & menus** — Registered in `package.json`, wired to handlers that
   call the server and inject results into the editor or chat panel.
4. **SafeType leak detection** — Scans all outbound chat messages and config
   files for accidentally included secrets (see Section 2.1)

### 2.1 SafeType Integration

MCP Universal handles API keys for 9+ providers. Users will paste code, write
config files, and chat with AI — all contexts where secrets can accidentally
leak. We integrate [SafeType](https://github.com/tsunipun/safetype) as a
built-in safety layer.

**What SafeType provides:** A platform-agnostic `Detector` class
(`@safetype/core`) that scans text against regex rules with context-aware
confidence scoring. It detects OpenAI keys, AWS keys, JWTs, private keys,
emails, and more.

**Where we hook it in:**

```
User types in chat panel
        │
        ▼
 ┌──────────────┐     ┌─────────────────────┐
 │  Chat Input   │────▶│  SafeType Detector   │
 │  (webview)    │     │  scan(text)          │
 └──────────────┘     └──────┬──────────────┘
                              │
                    ┌─────────▼─────────┐
                    │ Secret detected?   │
                    └─────────┬─────────┘
                       No     │     Yes
                       │      │      │
                       ▼      │      ▼
                 Send to      │  ⚠ Block + show warning
                 server       │  "API key detected — send anyway?"
                              │
                              ▼
                     User confirms → send (or cancel)
```

**Integration points:**

| Where | What happens | Phase |
|---|---|---|
| Chat panel (pre-send) | Scan user message before sending to server; block if secret found | Phase 2 |
| Config file diagnostics | VS Code diagnostics on `config.toml` — warns if keys are hardcoded | Phase 2 |
| AI response display | Scan AI responses for leaked secrets (model regurgitation) | Phase 2 |
| Editor file scanning | Full SafeType diagnostics on any open file (inherited from SafeType VSCode) | Phase 2 |

**Extended detection rules** — SafeType ships with rules for OpenAI and AWS
keys. We extend it with patterns for every provider MCP Universal supports:

| Provider | Pattern | Example |
|---|---|---|
| OpenAI | `sk-[a-zA-Z0-9]{32,}` | `sk-proj-abc123...` |
| Anthropic | `sk-ant-[a-zA-Z0-9-]{32,}` | `sk-ant-api03-...` |
| Gemini | `AIza[a-zA-Z0-9_-]{35}` | `AIzaSyB...` |
| HuggingFace | `hf_[a-zA-Z0-9]{34}` | `hf_abc123...` |
| Replicate | `r8_[a-zA-Z0-9]{40}` | `r8_abc123...` |
| RunPod | `rpa_[a-zA-Z0-9]{14,}` | `rpa_abc123...` |
| AWS | `AKIA[0-9A-Z]{16}` | `AKIAIOSFODNN7...` |
| JWT | `eyJ...` (three dot-separated base64 segments) | `eyJhbGciOi...` |
| Private Key | `-----BEGIN PRIVATE KEY-----` | PEM block |

---

## 3. Technology Stack

| Component | Technology | Why this choice |
|---|---|---|
| Web server | Axum 0.7 | Best Rust web framework for tower middleware + SSE |
| Async runtime | Tokio | Industry standard, required by Axum |
| HTTP client | Reqwest | Async, streaming body support, TLS built-in |
| SSE streaming | `axum::response::Sse` + `async-stream` | Native Axum SSE, zero extra deps |
| Serialization | Serde + serde_json | Standard Rust JSON |
| Config | `config` crate | Layered TOML + env merge built-in |
| Logging | `tracing` + `tracing-subscriber` | Structured, filterable, async-safe |
| CLI args | `clap` | Port/host/config-path overrides |
| Extension | TypeScript + VS Code Extension API | Required by VSCode |
| Webview UI | Vanilla HTML/CSS/JS | No bundler needed, fast load |
| Secret detection | `@safetype/core` | Scan chat input, config files, and AI responses for leaked secrets |

---

## 4. API Specification

### 4.1 Endpoints

| Method | Path | Streaming | Purpose |
|---|---|---|---|
| `POST` | `/mcp/chat` | No | Full response in one JSON body |
| `POST` | `/sse/chat` | Yes (SSE) | Token-by-token streaming |
| `GET` | `/providers` | No | List enabled providers |
| `GET` | `/providers/{id}/models` | No | List models for a provider |
| `GET` | `/health` | No | Liveness + per-provider status |

### 4.2 Request Schema — `/mcp/chat` and `/sse/chat`

```json
{
  "provider": "ollama",
  "model": "llama3",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user", "content": "Explain ownership in Rust." }
  ],
  "temperature": 0.7,
  "max_tokens": 2048
}
```

- `provider` — required, must match a key in the provider registry
- `model` — required, passed through to the backend
- `messages` — required, array of `{role, content}` objects
- `temperature` — optional, float 0.0–2.0, default per provider
- `max_tokens` — optional, default per provider

### 4.3 Response Schema — `/mcp/chat`

```json
{
  "provider": "ollama",
  "model": "llama3",
  "content": "Ownership is Rust's memory management system...",
  "usage": {
    "prompt_tokens": 24,
    "completion_tokens": 186,
    "total_tokens": 210
  }
}
```

### 4.4 SSE Event Schema — `/sse/chat`

```
event: token
data: {"content": "Ownership"}

event: token
data: {"content": " is"}

event: done
data: {"usage": {"prompt_tokens": 24, "completion_tokens": 186}}

event: error
data: {"message": "provider timeout", "code": "PROVIDER_TIMEOUT"}
```

Three event types: `token` (incremental text), `done` (final summary),
`error` (terminal failure — closes the stream).

### 4.5 Error Responses

All non-streaming errors return a consistent JSON envelope:

```json
{
  "error": {
    "code": "PROVIDER_UNAVAILABLE",
    "message": "Ollama is not reachable at http://localhost:11434",
    "provider": "ollama"
  }
}
```

Standard error codes:

| Code | HTTP Status | Meaning |
|---|---|---|
| `INVALID_REQUEST` | 400 | Missing/malformed fields |
| `UNKNOWN_PROVIDER` | 400 | Provider key not in registry |
| `UNKNOWN_MODEL` | 400 | Model not found for provider |
| `PROVIDER_UNAVAILABLE` | 502 | Upstream provider unreachable |
| `PROVIDER_ERROR` | 502 | Upstream returned an error |
| `PROVIDER_TIMEOUT` | 504 | Upstream did not respond in time |
| `RATE_LIMITED` | 429 | Too many requests (Phase 3) |
| `UNAUTHORIZED` | 401 | Missing/invalid auth token (Phase 3) |

---

## 5. Transport Modes

The MCP protocol defines multiple transports. SSE works well locally but
breaks when remote clients (including cloud-hosted MCP clients like OpenAI)
need to reach your server — `localhost` is unreachable from the internet, and
many clients enforce HTTPS. The server must support multiple transports to
cover all deployment scenarios.

### 5.1 Transport Comparison

| Transport | How it works | Domain needed? | Best for |
|---|---|---|---|
| **Stdio** | Server communicates over stdin/stdout, no network at all | No | Single-user local use; standard MCP client integration (Claude Desktop, etc.) |
| **SSE** (current) | Client GETs `/sse` for events, POSTs to `/messages` | Only if client is remote | Local VSCode extension; local MCP clients |
| **Streamable HTTP** | Single `POST` endpoint; server responds with SSE stream in the HTTP response body | Only if client is remote | Modern replacement for SSE transport; simpler, no endpoint discovery |
| **SSE + Tunnel** | SSE but server is exposed via ngrok / Cloudflare Tunnel | Tunnel provides URL | Quick remote access during development |

### 5.2 Implementation Plan

**Phase 1 (MVP):** SSE transport only. The extension and server both run
locally, so `localhost` works. The extension POSTs to `/sse/chat` and
receives tokens in the response SSE stream (this is effectively Streamable
HTTP for our custom endpoints).

**Phase 2 (Stability):** Add a `--public-url` CLI flag. When set, the server
advertises this URL (instead of `localhost`) in MCP SSE endpoint discovery
events. This makes it work behind tunnels and reverse proxies.

```bash
# Behind ngrok
ngrok http 3333
cargo run --release --public-url https://abc123.ngrok.io
```

**Phase 3 (Features):** Add **stdio transport** so the server can be launched
directly by MCP clients (Claude Desktop, OpenAI tool-use, etc.) without any
network setup:

```json
// Claude Desktop config — no domain, no port, no network
{
  "mcpServers": {
    "mcp-universal": {
      "command": "/path/to/mcp-universal",
      "args": ["--transport", "stdio"]
    }
  }
}
```

Also add **Streamable HTTP** transport (the newer MCP standard) as the
recommended network transport, keeping SSE for backward compatibility.

### 5.3 Why This Matters

The SSE issues you hit with OpenAI happen because:

1. **OpenAI as MCP client** needs to connect *to* your server — it can't
   reach `localhost`. Stdio transport eliminates this entirely (no network).
2. **SSE endpoint discovery** — the server sends back a URL for the client to
   POST to. Without `--public-url`, it sends `http://localhost:3333/messages`,
   which is useless to a remote client.
3. **TLS requirement** — many cloud MCP clients reject plain HTTP SSE. A
   tunnel (ngrok/Cloudflare) or a real domain with TLS solves this.

With all three transports, every deployment scenario is covered:

| Scenario | Transport | Domain? |
|---|---|---|
| You + VSCode on your laptop | SSE or Streamable HTTP | No |
| Claude Desktop on your machine | Stdio | No |
| Claude Code on your machine | Stdio | No |
| Codex on your machine | Stdio | No |
| Any local MCP client | Stdio or `localhost` HTTP | No |
| Remote MCP client | Streamable HTTP + tunnel or domain | Yes |
| Shared team server on LAN | SSE or Streamable HTTP | No (use IP) |
| Public deployment | Streamable HTTP + HTTPS | Yes |

---

## 6. Provider Trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    /// Unique key used in requests and config (e.g. "ollama", "openai").
    fn id(&self) -> &str;

    /// Human-readable name for UI display.
    fn display_name(&self) -> &str;

    /// Check if the provider is reachable and authenticated.
    async fn health_check(&self) -> ProviderHealth;

    /// List available models.
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;

    /// Non-streaming chat completion.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse>;

    /// Streaming chat completion — returns a stream of string tokens.
    fn chat_stream(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>>;
}
```

Each provider lives in its own module under `src/providers/` and implements
this trait. Adding a new OpenAI-compatible backend (LM Studio, vLLM, Together)
means adding one file and registering it in the provider registry.

---

## 7. Configuration

### 7.1 Layering Order (last wins)

1. Compiled defaults
2. `config.toml` (optional, path overridable via `--config`)
3. Environment variables (prefix `MCP_` for server, provider-specific for keys)

### 7.2 Full Config Schema

```toml
# Server
[server]
host = "127.0.0.1"         # MCP_SERVER_HOST
port = 3333                 # MCP_SERVER_PORT
request_timeout_secs = 60   # MCP_SERVER_REQUEST_TIMEOUT_SECS

# Ollama (local)
[providers.ollama]
enabled = true
base_url = "http://localhost:11434"   # OLLAMA_BASE_URL
default_model = "llama3"
request_timeout_secs = 120

# Ollama (remote)
[providers.ollama_remote]
enabled = false
base_url = ""               # OLLAMA_REMOTE_BASE_URL
default_model = "llama3"

# OpenAI
[providers.openai]
enabled = true              # auto-enabled when OPENAI_API_KEY is set
api_key = ""                # OPENAI_API_KEY
default_model = "gpt-4o"
request_timeout_secs = 90

# Gemini
[providers.gemini]
enabled = true              # auto-enabled when GEMINI_API_KEY is set
api_key = ""                # GEMINI_API_KEY
default_model = "gemini-2.0-flash"

# Claude
[providers.claude]
enabled = true              # auto-enabled when ANTHROPIC_API_KEY is set
api_key = ""                # ANTHROPIC_API_KEY
default_model = "claude-sonnet-4-20250514"

# OpenRouter (multi-model gateway — access 200+ models with one API key)
[providers.openrouter]
enabled = true              # auto-enabled when OPENROUTER_API_KEY is set
api_key = ""                # OPENROUTER_API_KEY
default_model = "openai/gpt-4o"

# HuggingFace Inference (Phase 3)
[providers.huggingface]
enabled = false             # auto-enabled when HUGGINGFACE_API_KEY is set
api_key = ""                # HUGGINGFACE_API_KEY
default_model = "meta-llama/Llama-3.1-70B-Instruct"

# Replicate (Phase 3)
[providers.replicate]
enabled = false             # auto-enabled when REPLICATE_API_TOKEN is set
api_key = ""                # REPLICATE_API_TOKEN
default_model = "meta/llama-3.1-405b-instruct"

# RunPod (Phase 3)
[providers.runpod]
enabled = false             # auto-enabled when RUNPOD_API_KEY is set
api_key = ""                # RUNPOD_API_KEY
endpoint_id = ""            # RUNPOD_ENDPOINT_ID
default_model = ""

# Generic OpenAI-compatible (Phase 3) — works with LM Studio, vLLM, Together, Groq, etc.
[providers.openai_compat]
enabled = false
base_url = ""               # OPENAI_COMPAT_BASE_URL (e.g. http://localhost:1234/v1)
api_key = ""                # OPENAI_COMPAT_API_KEY (optional, depends on backend)
default_model = ""
```

**Auto-enable rule:** If a cloud provider's API key env var is set but the
provider section is missing from TOML, the provider is automatically enabled
with defaults. This is what makes `cargo run` + env vars work with zero config.

---

## 8. Project Structure

```
rust-mcp-vscode-ext/
├── Cargo.toml
├── config.toml.example
├── src/
│   ├── main.rs              # CLI parsing, server startup
│   ├── config.rs            # Config structs + layered loading
│   ├── state.rs             # AppState, provider registry builder
│   ├── routes/
│   │   ├── mod.rs
│   │   ├── chat.rs          # POST /mcp/chat
│   │   ├── sse.rs           # POST /sse/chat
│   │   ├── providers.rs     # GET /providers, GET /providers/:id/models
│   │   └── health.rs        # GET /health
│   ├── providers/
│   │   ├── mod.rs           # Provider trait + shared types
│   │   ├── ollama.rs        # Ollama local
│   │   ├── ollama_remote.rs # Ollama remote
│   │   ├── openai.rs        # OpenAI
│   │   ├── gemini.rs        # Google Gemini
│   │   ├── claude.rs        # Anthropic Claude
│   │   ├── openrouter.rs    # OpenRouter (multi-model gateway)
│   │   ├── huggingface.rs   # HuggingFace Inference API (Phase 3)
│   │   ├── replicate.rs     # Replicate (Phase 3)
│   │   ├── runpod.rs        # RunPod Serverless (Phase 3)
│   │   └── openai_compat.rs # Generic OpenAI-compatible (Phase 3)
│   ├── transport/
│   │   ├── mod.rs           # Transport enum + shared types
│   │   ├── sse.rs           # Legacy SSE transport (MCP protocol)
│   │   ├── streamable.rs    # Streamable HTTP transport (MCP protocol)
│   │   └── stdio.rs         # Stdio transport for direct MCP client launch
│   ├── models.rs            # ChatRequest, ChatResponse, Message, etc.
│   └── error.rs             # AppError enum → Axum IntoResponse
├── tests/
│   ├── common/
│   │   └── mock_server.rs   # Shared mock HTTP server for provider tests
│   ├── test_ollama.rs
│   ├── test_openai.rs
│   ├── test_gemini.rs
│   ├── test_claude.rs
│   └── test_sse.rs
├── vscode-extension/
│   ├── package.json
│   ├── tsconfig.json
│   ├── src/
│   │   ├── extension.ts     # Activation, command registration
│   │   ├── client.ts        # HTTP + SSE client for the MCP server
│   │   ├── chat/
│   │   │   ├── panel.ts     # Webview panel lifecycle
│   │   │   ├── index.html   # Chat UI shell
│   │   │   ├── style.css    # Chat styling (VSCode theme-aware)
│   │   │   └── chat.js      # Webview-side JS (message handling, rendering)
│   │   ├── commands/
│   │   │   ├── complete.ts  # Cmd+Shift+K — complete selection
│   │   │   ├── explain.ts   # Right-click — explain selection
│   │   │   └── generate.ts  # Generate tests (Phase 3)
│   │   ├── safetype/
│   │   │   ├── detector.ts  # @safetype/core Detector wrapper
│   │   │   └── rules.ts     # Extended rules (Anthropic, Gemini, HF, Replicate, RunPod keys)
│   │   └── util/
│   │       ├── config.ts    # Read extension settings
│   │       └── markdown.ts  # Render markdown in webview
│   └── media/
│       └── icon.png
├── .github/
│   └── workflows/
│       ├── ci.yml           # Build + test on push/PR
│       └── release.yml      # Build binaries + publish on tag
├── Dockerfile
├── PLAN.md
├── LICENSE
└── README.md
```

---

## 9. Roadmap

### Phase 1 — MVP (February 2026) ✓ Target

| # | Task | Acceptance Criteria |
|---|---|---|
| 1.1 | Scaffold Rust project with Axum, Tokio, Serde, Clap | `cargo build` succeeds, server starts on port 3333 |
| 1.2 | Define `Provider` trait + `ChatRequest`/`ChatResponse` models | Types compile, serde round-trips pass |
| 1.3 | Implement Ollama local provider | `POST /mcp/chat` with `provider=ollama` returns a response |
| 1.4 | Implement SSE streaming endpoint | `POST /sse/chat` streams tokens to curl |
| 1.5 | Implement Ollama remote provider | Same as 1.3 but against a remote Ollama URL |
| 1.6 | Implement OpenAI provider | Chat + streaming work with `gpt-4o` |
| 1.7 | Implement Gemini provider | Chat + streaming work with `gemini-2.0-flash` |
| 1.8 | Implement Claude provider | Chat + streaming work with `claude-sonnet-4-20250514` |
| 1.9 | Config loader (TOML + env vars) | Server starts with no config file if env vars are set |
| 1.10 | `GET /providers` + `GET /providers/:id/models` | Returns JSON list of enabled providers and their models |
| 1.11 | Scaffold VSCode extension | `yo code` output compiles, activates in Extension Host |
| 1.12 | Chat panel webview | Cmd+Shift+A opens panel, messages render with markdown |
| 1.13 | Provider & model picker in chat panel | Dropdown fetches live from `/providers` and `/providers/:id/models` |
| 1.14 | SSE streaming in chat panel | Tokens appear in real time as they arrive |
| 1.15 | Complete selection command (Cmd+Shift+K) | Selected code is sent to server, response replaces selection |
| 1.16 | Explain selection command (right-click) | Explanation appears in chat panel |

### Phase 2 — Stability (March 2026)

| # | Task | Acceptance Criteria |
|---|---|---|
| 2.1 | Retry logic with exponential backoff | Flaky provider recovers within 3 retries; configurable per provider |
| 2.2 | Structured error propagation | Provider errors surface as typed `AppError` with correct HTTP status |
| 2.3 | Per-provider request timeout | Timeout is configurable in TOML/env; times out with `PROVIDER_TIMEOUT` |
| 2.4 | `GET /health` endpoint | Returns 200 with per-provider status object |
| 2.5 | Integration tests (all 5 providers) | Tests use mock HTTP servers; run in CI without real API keys |
| 2.6 | SSE disconnect handling in extension | Auto-reconnect on drop; user sees "Reconnecting..." message |
| 2.7 | Loading spinner in chat panel | Spinner visible during generation, disappears on `done`/`error` |
| 2.8 | Copy response button | One-click copy of any assistant message |
| 2.9 | Persist chat history | History survives VSCode restart via `globalState` |
| 2.10 | GitHub Actions CI | `ci.yml` runs `cargo test`, `cargo clippy`, `cargo fmt --check` on every PR |
| 2.11 | `--public-url` CLI flag | When set, SSE endpoint discovery advertises the public URL instead of localhost |
| 2.12 | ~~Tunnel documentation~~ ✓ | README section showing ngrok/Cloudflare Tunnel setup for remote MCP clients |
| 2.13 | Integrate SafeType core detection | `@safetype/core` added as dependency; `Detector` scans chat input before sending | 
| 2.14 | Extend SafeType rules for all providers | Detection rules for Anthropic (`sk-ant-`), Gemini (`AIza`), HF (`hf_`), Replicate (`r8_`), RunPod keys |
| 2.15 | Chat panel leak warning UI | Blocked message with inline warning when secret detected; user can override to send anyway |
| 2.16 | Config file scanning | SafeType diagnostics on `config.toml` — warns if API keys are hardcoded instead of using env vars |

### Phase 3 — Features (April 2026)

| # | Task | Acceptance Criteria |
|---|---|---|
| 3.1 | Optional Bearer token auth on the MCP server | Requests without valid token get 401 |
| 3.2 | Per-provider rate limiting | Configurable RPM; excess requests get 429 |
| 3.3 | ~~Multi-turn context management~~ ✓ | Old messages auto-trimmed to fit context window; strategy configurable |
| 3.4 | Generic OpenAI-compatible provider | Works with LM Studio, vLLM, Together, Groq by setting a base URL |
| 3.5 | HuggingFace Inference provider | Chat + streaming work with HF Inference API models |
| 3.6 | Replicate provider | Chat + streaming work via Replicate predictions API |
| 3.7 | RunPod provider | Chat + streaming work via RunPod serverless endpoints |
| 3.8 | Docker image | `docker build` produces image; published to GHCR on tag |
| 3.9 | Inline ghost text suggestions | Completions appear as ghost text at cursor; Tab to accept |
| 3.10 | Insert response into editor | Button in chat panel inserts AI response at cursor position |
| 3.11 | File context in chat | Open file content automatically attached; toggle on/off |
| 3.12 | @mention files/symbols | Type `@` in chat to reference files; content included in prompt |
| 3.13 | ~~Generate unit tests for selection~~ ✓ | Command generates tests, opens in new editor tab |
| 3.14 | Stdio transport | `--transport stdio` mode for direct MCP client integration (Claude Desktop, Claude Code, Codex, etc.) — no network required |
| 3.15 | Streamable HTTP transport | Single POST endpoint returns SSE in response body; replaces legacy SSE transport for network use |
| 3.16 | MCP protocol compliance | Server implements MCP `initialize`, `tools/list`, `tools/call` lifecycle over all transports |

### Phase 4 — Release (May 2026)

| # | Task | Acceptance Criteria |
|---|---|---|
| 4.1 | ~~README with quickstart, screenshots, config reference~~ ✓ | New user can go from zero to chatting in < 5 minutes |
| 4.2 | ~~CONTRIBUTING.md + issue templates~~ ✓ | Clear process for PRs and bug reports |
| 4.3 | ~~Release workflow~~ ✓ | `git tag v1.0.0` → binaries for Linux x86/ARM, macOS, Windows |
| 4.4 | ~~Publish extension to VS Code Marketplace~~ ✓ | README documents publish workflow; CI release packages `.vsix` |
| 4.5 | Documentation site | GitHub Pages with full API reference and guides |
| 4.6 | Community launch | Posts on Hacker News, r/rust, r/LocalLLaMA, X/Twitter |

---

## 10. Testing Strategy

| Layer | Tool | What's Tested |
|---|---|---|
| Unit tests | `cargo test` | Config parsing, model serialization, error mapping |
| Provider integration | `cargo test` + `wiremock` | Each provider against a mock HTTP server — request format, streaming parse, error handling |
| SSE integration | `cargo test` + `reqwest` | Full round-trip: send request → receive SSE events → validate sequence |
| Extension unit | Mocha + sinon | Client module, message parsing, config reading |
| SafeType detection | Mocha | All extended rules detect correct patterns with expected confidence; no false positives on normal code |
| Extension E2E | `@vscode/test-electron` | Activate extension, open chat panel, verify commands register |
| Manual smoke test | Checklist | Each provider with a real API key; streaming and non-streaming |

**CI rule:** All unit and integration tests must pass before merge. No real API
keys in CI — every provider test uses mocks.

---

## 11. Security Considerations

| Concern | Mitigation |
|---|---|
| API keys in config file | TOML file is `.gitignore`d; env vars are the recommended path; SafeType warns on hardcoded keys |
| Secrets leaked in chat messages | SafeType scans all outbound messages pre-send; blocks with warning if secret detected |
| Secrets in AI responses | SafeType scans model responses for regurgitated keys (model regurgitation attack) |
| API keys in transit (ext → server) | Server binds to `127.0.0.1` by default; keys never leave the server |
| Prompt injection | Server is a transparent proxy — no system prompts are injected server-side |
| MCP server auth (Phase 3) | Optional Bearer token; required when `server.auth_token` is set |
| Dependency supply chain | `cargo audit` in CI; minimal dependency tree |
| CORS | Restricted to VSCode webview origin by default |

---

## 12. Performance Targets

| Metric | Target | How to Measure |
|---|---|---|
| Cold start | < 50 ms | Time from process start to "listening on" log |
| First token latency (SSE) | < network RTT + 200 ms | Timestamp of first SSE event minus request send time |
| Memory at idle | < 10 MB RSS | `ps` or `/proc` after startup with no active requests |
| Concurrent streams | 100+ simultaneous | `k6` or `wrk` load test with mock provider |
| Binary size (release) | < 15 MB | `ls -lh target/release/mcp-universal` |

---

## 13. Open Decisions

These are unresolved questions that should be decided before or during Phase 3.

| # | Question | Leaning | Decide By |
|---|---|---|---|
| D1 | Support generic OpenAI-compatible endpoints? | **Yes** — covers LM Studio, vLLM, Together, Groq | Phase 3 start |
| D2 | Expose MCP tools to models with function calling? | **Defer** — valuable but large scope; consider as Phase 5 | Phase 3 end |
| D3 | GUI config editor in the VSCode extension? | **Yes** — settings UI that writes to `config.toml` | Phase 3 |
| D4 | Neovim / JetBrains plugins? | **Post-v1** — server is editor-agnostic, clients can come later | Phase 4 end |
| D5 | Opt-in telemetry? | **No for v1** — keep trust high; revisit if community requests it | Phase 4 |
| D6 | WebSocket as alternative to SSE? | **No** — Streamable HTTP (Phase 3) is the modern MCP replacement; WebSocket adds complexity with no benefit | Phase 2 |
| D7 | Which transport is the default? | **SSE** in Phase 1–2, **Streamable HTTP** from Phase 3 onward; stdio always available via `--transport stdio` | Phase 3 start |

---

## 14. Risks & Mitigations

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| Ollama API changes | Breaks local provider | Medium | Pin tested Ollama version in docs; abstract API behind version-checked adapter |
| OpenAI/Anthropic deprecate models | User-facing errors | Medium | Model listing is dynamic; server doesn't hardcode model names |
| VSCode webview API changes | Extension breaks | Low | Lock `@types/vscode` engine version; test against stable + insiders |
| Streaming parse errors | Garbled output | Medium | Strict SSE parser with per-event error boundaries; malformed events logged and skipped |
| SSE transport fails with remote MCP clients | Users can't connect OpenAI, Claude Desktop, etc. | High | Stdio transport (Phase 3) eliminates the problem entirely; `--public-url` + tunnel docs (Phase 2) as interim fix |
| Scope creep | Missed timelines | High | Ruthless phase gating — nothing moves to next phase until current is stable |

---

## 15. Getting Started (Developer Quickstart)

### Run the server

```bash
# Clone and build
git clone https://github.com/<you>/rust-mcp-vscode-ext.git
cd rust-mcp-vscode-ext
cargo run --release

# The server starts on http://127.0.0.1:3333
# Ollama provider is enabled by default (if Ollama is running)
```

### Add a cloud provider

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GEMINI_API_KEY=AIza...
cargo run --release
# All three cloud providers auto-enable — no config.toml needed
```

### Install the VSCode extension

```bash
cd vscode-extension
npm install && npm run compile
# Press F5 in VSCode to launch Extension Development Host
# Cmd+Shift+A → Chat panel opens
# Select provider → start chatting
```

### Run tests

```bash
cargo test                        # Server unit + integration tests
cd vscode-extension && npm test   # Extension tests
```

---

*Last updated: February 2026*
