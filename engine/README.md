# MCP Universal Agent

An all-in-one AI coding agent powered by Rust. Connects any LLM provider to your codebase via MCP (Model Context Protocol), with browser automation, messaging adapters, RAG-based memory, and a VSCode extension.

## Features

- **Multi-provider LLM support** — OpenAI, Claude, Gemini, Grok (xAI), DeepSeek, Ollama (local & remote), OpenRouter (200+ models), HuggingFace, Replicate, RunPod, any OpenAI-compatible API (LM Studio, vLLM, Together, Groq, etc.)
- **Agent runtime** — Tool-calling loop with workspace tools (read/write/patch files, search, git), shell commands, and browser automation
- **MCP protocol** — JSON-RPC 2.0 server over HTTP, Streamable HTTP, and stdio, compatible with any MCP client
- **RAG memory** — SQLite-backed vector store with hybrid search (FTS5 + embeddings), AST-aware code chunking, tiered memory, and persistent facts
- **Context management** — Automatic conversation trimming to fit model context windows with per-model limits
- **Project summarizer** — Auto-scans your codebase on startup, indexes entry points and key files
- **File watcher** — Incremental re-indexing when files change
- **Tool approval** — Destructive operations require explicit user approval via VSCode or the API
- **Auth & accounts** — Bearer token auth, JWT-based user accounts, subscription tiers (Free/Pro/Enterprise), per-request and per-account BYOK
- **Credits & billing** — Pay-per-use credit system with configurable markup, usage tracking, and quota enforcement
- **Per-provider rate limiting** — Sliding-window rate limiter configurable per provider
- **Docker support** — Multi-stage Dockerfile for lightweight deployment
- **Token streaming** — Real-time SSE streaming for chat and agent tool-calling
- **Messaging adapters** — Telegram, Discord, Slack, WhatsApp bots with per-user auth and rate limiting
- **CLI mode** — Interactive terminal agent
- **SafeType** — Secret detection in chat inputs and config files, preventing accidental API key leaks
- **VSCode extension** — Chat panel with streaming, agent mode, tool call visualization, approval dialogs, inline completions, test generation, and `@mention` file context

## Quick Start

```bash
# Build the server
cargo build --release

# Run with default config (Ollama on localhost)
./target/release/mcp-universal

# Or with env vars for cloud providers
OPENAI_API_KEY=sk-... ./target/release/mcp-universal

# CLI agent mode
OPENAI_API_KEY=sk-... ./target/release/mcp-universal --cli --provider openai

# MCP stdio mode (for editor integrations)
./target/release/mcp-universal --stdio

# Docker
docker build -t mcp-universal .
docker run -p 3333:3333 -e OPENAI_API_KEY=sk-... mcp-universal
```

## Tunnels & Public Access

To expose your local MCP server to the internet (for messaging bots, remote clients, etc.), use a tunnel service:

### ngrok

```bash
# Install ngrok and authenticate
ngrok http 3333

# Set the public URL so SSE clients get correct endpoints
MCP_PUBLIC_URL=https://abc123.ngrok.io ./target/release/mcp-universal
```

### Cloudflare Tunnel

```bash
# One-time tunnel (no account needed)
cloudflared tunnel --url http://localhost:3333

# Persistent tunnel with a custom domain
cloudflared tunnel create mcp-agent
cloudflared tunnel route dns mcp-agent mcp.yourdomain.com
cloudflared tunnel run mcp-agent
```

### Tailscale Funnel

```bash
tailscale funnel 3333
```

Set `MCP_PUBLIC_URL` to the tunnel URL so the server includes it in SSE responses and webhook registrations. When using `MCP_AUTH_TOKEN`, the tunnel is safe — all endpoints except `/health` require the Bearer token.

## Configuration

Copy `config.toml.example` to `config.toml` and edit, or use environment variables (env vars always override file values).

### Environment Variables

| Variable | Description |
|---|---|
| `MCP_SERVER_HOST` | Bind address (default: `127.0.0.1`) |
| `MCP_SERVER_PORT` | Port (default: `3333`) |
| `MCP_PUBLIC_URL` | Public URL for SSE clients behind a tunnel |
| `MCP_AGENT_PROJECT_ROOT` | Agent workspace root (default: `.`) |
| `MCP_AGENT_MAX_ITERATIONS` | Max tool-calling iterations (default: `50`) |
| `OPENAI_API_KEY` | Enables OpenAI provider |
| `ANTHROPIC_API_KEY` | Enables Claude provider |
| `GEMINI_API_KEY` | Enables Gemini provider |
| `OPENROUTER_API_KEY` | Enables OpenRouter provider |
| `XAI_API_KEY` | Enables Grok (xAI) provider |
| `DEEPSEEK_API_KEY` | Enables DeepSeek provider |
| `MCP_AUTH_TOKEN` | Bearer token for API auth (all endpoints except `/health`) |
| `OLLAMA_BASE_URL` | Custom Ollama URL (default: `localhost:11434`) |
| `MCP_MEMORY_ENABLED` | Enable RAG memory (`true`/`false`) |
| `MCP_MEMORY_EMBEDDING_PROVIDER` | `openai`, `gemini`, or `ollama` |
| `MCP_MEMORY_EMBEDDING_MODEL` | Override embedding model |
| `TELEGRAM_BOT_TOKEN` | Enables Telegram adapter |
| `DISCORD_BOT_TOKEN` | Enables Discord adapter |
| `SLACK_BOT_TOKEN` | Enables Slack adapter |
| `WHATSAPP_API_TOKEN` | Enables WhatsApp adapter |
| `WHATSAPP_PHONE_NUMBER_ID` | WhatsApp Business phone number ID |
| `WHATSAPP_VERIFY_TOKEN` | Webhook verification token |
| `OPENAI_COMPAT_BASE_URL` | Base URL for OpenAI-compatible API (auto-creates provider) |
| `OPENAI_COMPAT_API_KEY` | API key for OpenAI-compatible provider (optional) |
| `OPENAI_COMPAT_DEFAULT_MODEL` | Default model for OpenAI-compatible provider |
| `HF_API_KEY` | Enables HuggingFace Inference provider |
| `REPLICATE_API_TOKEN` | Enables Replicate provider |
| `RUNPOD_API_KEY` | Enables RunPod Serverless provider |
| `RUNPOD_ENDPOINT_ID` | RunPod endpoint ID (required with `RUNPOD_API_KEY`) |
| `MCP_ACCOUNTS_ENABLED` | Enable user accounts (`true`/`false`) |
| `MCP_JWT_SECRET` | Secret for signing JWTs (required if accounts enabled) |

## API Endpoints

| Method | Path | Description |
|---|---|---|
| `POST` | `/mcp/chat` | Non-streaming chat |
| `POST` | `/sse/chat` | Streaming chat (SSE) |
| `POST` | `/mcp/stream` | MCP Streamable HTTP transport (SSE) |
| `POST` | `/agent/run` | Agent mode with tool calling (SSE) |
| `POST` | `/agent/approve` | Approve/deny a tool execution |
| `POST` | `/mcp/message` | MCP JSON-RPC message handler |
| `POST` | `/patch/preview` | Preview patch operations |
| `POST` | `/patch/apply` | Apply patch to files |
| `POST` | `/patch/revert/:id` | Revert a previously applied patch |
| `GET` | `/providers` | List available providers |
| `GET` | `/providers/:id/models` | List models for a provider |
| `GET` | `/health` | Health check |
| `POST` | `/accounts/register` | Create a new user account |
| `POST` | `/accounts/login` | Authenticate and get JWT |
| `GET` | `/accounts/me` | Get current user info |
| `POST` | `/accounts/keys` | Store a per-provider API key (BYOK) |
| `GET` | `/accounts/keys` | List stored API keys |
| `DELETE` | `/accounts/keys` | Remove a stored API key |
| `POST` | `/slack/events` | Slack Events API webhook |
| `GET` | `/whatsapp/webhook` | WhatsApp webhook verification |
| `POST` | `/whatsapp/webhook` | WhatsApp incoming messages |

## Agent Tools

| Tool | Approval Required | Description |
|---|---|---|
| `read_file` | No | Read file contents |
| `write_file` | Yes | Write/create files |
| `patch_file` | Yes | Search-and-replace edit |
| `list_directory` | No | List directory contents |
| `search_files` | No | Regex search across files |
| `git_status` | No | Git status |
| `git_diff` | No | Git diff |
| `git_log` | No | Git log |
| `git_add` | Yes | Stage files |
| `git_commit` | Yes | Commit changes |
| `run_command` | Yes | Execute allowlisted shell commands |
| `browser_navigate` | No | Navigate to URL |
| `browser_click` | No | Click element by selector |
| `browser_type` | No | Type text into element |
| `browser_screenshot` | No | Capture screenshot |
| `browser_scrape` | No | Scrape page content |

## VSCode Extension

```bash
cd vscode-extension
bun install
bun run build
```

Install the extension from `vscode-extension/dist/` or press F5 in VSCode to launch the Extension Development Host.

Commands:
- `MCP Universal: Open Chat` — Chat panel with streaming
- `MCP Universal: Open Agent` — Agent mode with tool calling
- `MCP Universal: Complete Selection` — Complete selected code in place
- `MCP Universal: Explain Selection` — Explain selected code in chat
- `MCP Universal: Generate Tests` — Generate unit tests for selection or file

Features:
- Inline ghost text completions (configurable provider/model)
- SafeType secret detection in chat and config files
- `@filename` mentions to include file context
- Attach files from workspace to chat
- Insert AI responses directly into editor

## Memory System

When memory is enabled, the agent maintains context across sessions:

- **Hybrid search** — Combines vector similarity (60%) with FTS5 keyword matching (40%)
- **Tiered memory** — Short (sessions), Medium (indexed files), Long (persistent facts + project summary)
- **Time decay** — Recent memories score higher (7-day half-life)
- **AST-aware chunking** — Code is split by function/class boundaries for Rust, TypeScript, Python, Go, and Java
- **Persistent facts** — Add a `MEMORY.md` file to your project root with facts the agent should always remember
- **Auto-indexing** — Project is scanned on startup; files are re-indexed on change via file watcher
- **Merkle-tree incremental indexing** — Content-hash driven; only re-embeds files that actually changed

## Development

```bash
# Run tests
cargo test

# Lint
cargo clippy --all-targets -- -D warnings

# Format
cargo fmt

# Extension lint + type check
cd vscode-extension && bun run lint && bunx tsc --noEmit
```

## Architecture

```
src/
├── accounts/       # User accounts, JWT auth, tiers, usage tracking, credits
├── agent/          # Agent runtime, session management, task queue
├── circuit_breaker.rs  # Circuit breaker for provider resilience
├── config.rs       # Configuration with env var overrides
├── context.rs      # Context window management and message trimming
├── error.rs        # Error types
├── mcp/            # MCP protocol, handler, stdio transport
├── memory/         # RAG: store, embeddings, indexer, chunker, summarizer, watcher
├── messaging/      # CLI, Telegram, Discord, Slack, WhatsApp adapters + rate limiter
├── models.rs       # Shared data types
├── patch/          # Patch engine for file operations with rollback
├── providers/      # LLM integrations (12+ providers)
├── rate_limit.rs   # Per-provider sliding-window rate limiter
├── retry.rs        # Retry with backoff
├── routes/         # HTTP handlers (chat, SSE, agent, MCP, accounts, patch, health)
├── state.rs        # Shared application state
└── tools/          # Tool trait, registry, workspace/git/command/browser/approval

vscode-extension/
├── src/chat/       # Chat panel webview
├── src/commands/   # Editor commands (complete, explain, generate tests, inline)
└── src/safetype/   # Secret detection rules and diagnostics
```

## Publishing the VSCode Extension

To publish to the VS Code Marketplace:

```bash
cd vscode-extension

# Install vsce
bun add -g @vscode/vsce

# Package as .vsix
bunx @vscode/vsce package --no-dependencies

# Publish (requires a Personal Access Token from https://dev.azure.com)
bunx @vscode/vsce publish -p <YOUR_PAT>
```

Or install locally from the `.vsix` file:

```bash
code --install-extension mcp-universal-0.1.0.vsix
```

## Releases

Binary releases for Linux (x86/ARM), macOS (x86/ARM), and Windows are built automatically by GitHub Actions when a version tag is pushed:

```bash
git tag v0.1.0
git push origin v0.1.0
```

This triggers the release workflow which builds binaries, packages the VSCode extension, and creates a GitHub Release with all artifacts.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and PR process.

## License

MIT
