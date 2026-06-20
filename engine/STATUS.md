# Project Status — MCP Universal

> Last updated: February 28, 2026  
> Build: `cargo check` ✅ | `bun run build` ✅ (50.0 kb)

---

## Table of Contents

1. [Features Available](#features-available)
2. [UI Status](#ui-status)
3. [Extension Status](#extension-status)
4. [What Is Left To Do](#what-is-left-to-do)

---

## Features Available

### API Endpoints

All routes are registered in `src/main.rs`.

#### Public / No Auth

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Always returns 200 |
| `POST` | `/auth/register` | Create account (bcrypt + SQLite) |
| `POST` | `/auth/login` | Returns JWT (HS256) |

#### Auth-Protected (Bearer JWT)

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/accounts/me` | Current user profile |
| `GET` | `/accounts/api-keys` | List stored provider keys |
| `POST` | `/accounts/api-keys` | Upsert a provider key |
| `DELETE` | `/accounts/api-keys/:id` | Remove a provider key |
| `GET` | `/accounts/credits` | Credit balance |
| `POST` | `/accounts/credits/purchase` | Purchase credits (⚠️ likely stub — no payment processor) |
| `POST` | `/chat` | Single-turn chat (non-streaming) |
| `POST` | `/sse` | SSE streaming chat |
| `POST` | `/agent` | SSE agent loop with tool use |
| `POST` | `/agent/approve` | Approve a pending tool call |
| `POST` | `/agent/orchestrate` | Multi-mode orchestrated agent |
| `GET` | `/providers` | List enabled providers |
| `GET` | `/providers/:name/models` | List models for a provider |
| `POST` | `/patch/preview` | Dry-run patch from LLM response |
| `POST` | `/patch/apply` | Apply patch from LLM response |
| `GET` | `/tasks` | List background planner tasks |
| `POST` | `/tasks` | Enqueue a planner task |
| `GET` | `/tasks/:id` | Poll task status |
| `POST` | `/tasks/:id/cancel` | Cancel a queued task |
| `POST` | `/mcp` | Single JSON-RPC over HTTP |
| `POST` | `/mcp/stream` | Streaming JSON-RPC over HTTP |

#### Optional (only if configured)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/webhooks/slack` | Slack events |
| `GET/POST` | `/webhooks/whatsapp` | WhatsApp verification + events |

---

### Agent System

| Component | File | Status |
|-----------|------|--------|
| **Agent modes** (Chat / Plan / Agent / Debug / Auto-detect) | `src/agent/modes.rs` | ✅ Fully implemented |
| **Agent runtime** (loop, tool dispatch, streaming) | `src/agent/runtime.rs` | ✅ Fully implemented |
| **Orchestrator** (wraps runtime for all modes) | `src/agent/orchestrator.rs` | ✅ Wired |
| **Task planner** (LLM → JSON plan with dependency graph) | `src/agent/planner.rs` | ✅ Plan generation works |
| **Task queue** (async MPSC, status tracking) | `src/agent/task_queue.rs` | ✅ Queue wired into state |
| **Tool timeout** (configurable, default 300 s) | `src/agent/runtime.rs` | ✅ tokio::time::timeout |
| **Circuit breaker** (5 fail → open, 5 min recovery) | `src/circuit_breaker.rs` | ✅ Used in agent loop |
| **Approval gate** (per-tool human confirm, 5 min timeout) | `src/tools/approval.rs` | ✅ No deadlock on timeout |
| **Retry + circuit breaker** (exponential backoff) | `src/retry.rs` | ✅ Used for provider calls |

---

### Tools

| Tool | File | Status |
|------|------|--------|
| `read_file`, `write_file`, `patch_file`, `list_directory`, `search_files` | `src/tools/workspace.rs` | ✅ |
| `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit` | `src/tools/git.rs` | ✅ |
| `run_command` (shell, allowlist, requires approval) | `src/tools/command.rs` | ✅ sandboxed |
| `browser_navigate/click/type/screenshot/scrape` | `src/tools/browser.rs` | ⚠️ Implemented but **never activated** in server mode |

---

### Memory System

| Component | File | Status |
|-----------|------|--------|
| SQLite vector store (hybrid BM25 + cosine) | `src/memory/store.rs` | ✅ |
| Embeddings (OpenAI / Gemini / Ollama) | `src/memory/embeddings.rs` | ✅ |
| Session indexer | `src/memory/indexer.rs` | ✅ |
| Context builder (recent-change boosting, project-graph boosting) | `src/memory/context_builder.rs` | ✅ |
| Project graph (Rust regex AST: symbols, imports) | `src/memory/project_graph.rs` | ✅ |
| Call graph (`syn`-based AST call graph) | `src/memory/call_graph.rs` | ✅ |
| File watcher (`notify` crate) | `src/memory/watcher.rs` | ✅ spawned on start |
| Project analysis cache (TTL + mtime invalidation) | `src/memory/cache.rs` | ✅ struct exists |
| Persistent facts store (key-value per project) | `src/memory/persistent.rs` | ✅ |
| Recent file tracker | `src/memory/recent_tracker.rs` | ✅ |
| Summarizer | `src/memory/summarizer.rs` | ✅ |

---

### Patch Engine

| Component | Status |
|-----------|--------|
| Unified diff parser (`@@ -x,y +x,y @@` with context validation) | ✅ |
| Multi-file dry-run preview | ✅ |
| Atomic apply (sequential per-file writes) | ✅ |
| Snapshot-based rollback | ✅ struct + method exist |
| `POST /patch/preview` route | ✅ |
| `POST /patch/apply` route | ✅ |
| LLM patch prompt templates | ✅ |
| JSON patch response extractor (handles fenced + raw) | ✅ |

---

### MCP Transport

| Transport | Status |
|-----------|--------|
| stdio (line-delimited JSON-RPC, 10 MB limit) | ✅ |
| HTTP POST (`/mcp`) single request | ✅ |
| HTTP streaming (`/mcp/stream`) | ✅ |
| MCP `tools/list` + `tools/call` | ✅ |
| MCP `resources` namespace | ❌ not implemented |
| MCP `prompts` namespace | ❌ not implemented |

---

### Providers

| Provider | Notes |
|----------|-------|
| Ollama (local) | Auto-registered, streaming + tools conditional |
| Ollama (remote) | Config only |
| OpenAI | Full — streaming + tool calling |
| Anthropic Claude | Full — streaming + tool calling |
| Google Gemini | Full — streaming + tool calling |
| xAI Grok | Partial (tool calling not confirmed) |
| DeepSeek | Partial |
| HuggingFace | Partial |
| OpenRouter | Partial |
| Replicate | Partial |
| RunPod | Partial |
| OpenAI-compatible | Configurable list |

---

### Auth / Accounts

| Feature | Status |
|---------|--------|
| Register / Login (bcrypt + JWT HS256) | ✅ |
| SQLite user DB with migrations | ✅ |
| JWT middleware (`extract_user`) | ✅ |
| Tier definitions (Free / Pro / Enterprise) | ✅ defined |
| Rate limits per tier | ✅ defined |
| Usage logging | ✅ defined |
| Provider key storage per user | ✅ stored in DB |
| Credit balance | ✅ stored |

---

### Messaging Bots

| Bot | Status |
|-----|--------|
| Slack | ✅ webhook handler wired |
| WhatsApp | ✅ webhook handler wired |
| Telegram | ⚠️ implemented, not wired to webhook |
| Discord | ⚠️ implemented, not wired to webhook |
| Bots bypass all auth / quota / tier | ⚠️ no auth enforcement |

---

### Tests

| File | Coverage Area |
|------|--------------|
| `test_approval.rs` | Approval gate |
| `test_config.rs` | Config loading |
| `test_error.rs` | Error types |
| `test_mcp_protocol.rs` | MCP JSON-RPC |
| `test_memory.rs` | Memory store / retrieval |
| `test_models.rs` | Serialization models |
| `test_providers_mock.rs` | Mock provider |
| `test_rate_limiter.rs` | Rate limiter |
| `test_retry.rs` | Retry + backoff |
| `test_routes.rs` | HTTP route handlers |
| `test_state.rs` | AppState construction |
| `test_tools_command.rs` | Shell command tool |
| `test_tools_git.rs` | Git tools |
| `test_tools_workspace.rs` | Workspace tools |

---

## UI Status

The UI lives entirely in `vscode-extension/src/chat/panel.ts` (1152 lines) as a VS Code Webview.

### What Works in the UI

| Feature | Status |
|---------|--------|
| Provider picker (dropdown) | ✅ |
| Model picker (dropdown, loaded from `/providers/:name/models`) | ✅ |
| Chat mode toggle (Chat / Agent) | ✅ |
| Agent mode selector (Auto / Ask / Plan / Agent / Debug) | ✅ |
| Chat history (persisted in `globalState`) | ✅ |
| Streaming chat with SSE | ✅ reconnects up to 3× with backoff |
| Tool-call display (collapsible) | ✅ |
| Tool result display | ✅ |
| Approval request UI (approve/deny pending tool) | ✅ |
| `@mention` file resolution (injects file content) | ✅ |
| Active file context injection | ✅ |
| File attachment | ✅ |
| Copy to clipboard | ✅ |
| Insert to editor | ✅ |
| Secret scanning before send (`SafeType`) | ✅ |
| Provider + model + mode persistence across sessions | ✅ `globalState` |

### What Does NOT Work Yet

| Feature | Status |
|---------|--------|
| Patch preview panel (`patchPreview.ts`) wired into chat | ❌ built but never triggered |
| Task queue status / progress display | ❌ no polling UI |
| Inline apply via `workspace.applyEdit()` | ❌ not implemented |
| Task progress in chat stream | ❌ not implemented |
| Diff view inline (before/after side-by-side) | ❌ not implemented |

---

## Extension Status

### Commands

| Command | Keybind | Status |
|---------|---------|--------|
| `mcpUniversal.openChat` | `⌘⇧A` | ✅ opens panel |
| `mcpUniversal.openAgent` | — | ✅ opens panel + switches to agent mode |
| `mcpUniversal.inlineCode` | `⌘⇧K` | ✅ sends selection, replaces inline |
| `mcpUniversal.chatWithSelection` | context menu | ✅ streams selection to panel |

### Settings (7 added in last session)

| Setting | Default | Status |
|---------|---------|--------|
| `mcpUniversal.serverUrl` | `http://localhost:3333` | ✅ |
| `mcpUniversal.chat.defaultProvider` | `""` | ✅ |
| `mcpUniversal.chat.defaultModel` | `""` | ✅ |
| `mcpUniversal.chat.defaultMode` | `chat` | ✅ |
| `mcpUniversal.agent.maxIterations` | `50` | ✅ |
| `mcpUniversal.agent.toolTimeout` | `300` | ✅ |
| `mcpUniversal.agent.autoMode` | `auto` | ✅ |

### Client (`client.ts`)

| Method | Status |
|--------|--------|
| `chat()` | ✅ |
| `chatStream()` (SSE) | ✅ |
| `agentStream()` (SSE) | ✅ |
| `completeInline()` | ✅ |
| `previewPatch()` | ✅ defined |
| `applyPatch()` | ✅ defined |

### Other Extension Pieces

| Component | Status |
|-----------|--------|
| Inline completions provider | ✅ 500 ms debounce, off by default |
| `SafeType` / secret diagnostics | ✅ activated at startup |
| Extension development host launch config | ✅ `.vscode/launch.json` |
| `PatchPreviewPanel` webview | ✅ built, **not wired to any command** |

---

## What Is Left To Do

### 🔴 Critical / Broken

| Item | Detail |
|------|--------|
| **Task execution is hollow** | `routes/tasks.rs` enqueues a task and the `TaskQueue` worker runs it, but the execution closure only constructs a `TaskPlanner` and iterates `next_steps()` — it never calls any tool, applies any patch, or runs any command. Plans are generated but not executed. |
| **Patch preview never shown** | `patchPreview.ts` (`PatchPreviewPanel`) is fully built but no code in the extension ever calls `PatchPreviewPanel.show()`. It cannot be reached by the user. |
| **Per-user API keys unused** | Users can save provider keys via `/accounts/api-keys` but those keys are never injected into provider instances at request time. Every request uses server-level env vars only. |
| **Tier / quota enforcement not enforced** | `UsageTracker::check_quota()` exists in `src/accounts/usage.rs` but is never called from any route or middleware. All users have unlimited access regardless of tier. |
| **Rollback snapshot discarded** | `apply_with_rollback()` returns a `Snapshot` but `routes/patch.rs` discards it with `let _`. There is no `/patch/revert` endpoint and no undo path. |
| **Single global approval gate** | `AppState` holds one `ApprovalGate`. If two agent sessions run concurrently, they stomp each other's approval requests. |

### 🟡 Missing / Incomplete Features

| Item | Effort | Detail |
|------|--------|--------|
| Wire `PatchPreviewPanel` into `panel.ts` | Small | Import it, add a `previewPatch` message case, call `PatchPreviewPanel.show()` |
| `/patch/revert` endpoint | Small | Store `Snapshot` in `AppState` (keyed by patch ID), expose `POST /patch/revert/:id` |
| Task queue polling UI | Medium | Poll `GET /tasks/:id` every 2 s in the extension while status is `Running`, display progress in panel |
| Inline apply via `workspace.applyEdit()` | Medium | Convert unified diff → `vscode.WorkspaceEdit`, apply without leaving editor |
| Enforce quota / tier in route middleware | Medium | Call `UsageTracker::check_quota()` in middleware or per-route, return 429 on limit |
| Inject per-user API keys into providers | Medium | At request time, read user's stored key for chosen provider and use it |
| Browser tools activation in server mode | Small | Call `browser_tools()` in tool registry setup |
| Share `ProjectAnalysisCache` in `AppState` | Medium | Add `Arc<ProjectAnalysisCache>` to `AppState`, pass to context builder |
| Fix empty `current_files` in `get_context()` | Small | Parse active file paths from session messages and pass to context builder |
| Token refresh endpoint | Small | `POST /auth/refresh` returning new JWT |
| MCP `resources` + `prompts` namespaces | Large | Add to `src/mcp/handler.rs` |
| Telegram + Discord webhooks | Medium | Add `POST /webhooks/telegram` and `POST /webhooks/discord` routes |
| Payment processor wiring | Large | Wire `/accounts/credits/purchase` to Stripe or similar |
| Password reset / email verification | Medium | Needs email sender, token store |

### 🟢 Nice-To-Have / Future

| Item | Detail |
|------|--------|
| **Tree-sitter multi-language AST** | Add `tree-sitter` crate + TypeScript/Python/Go grammars to replace regex-based project graph |
| **Fuzzy context matching in patch apply** | `apply_hunks()` does byte-exact context matching; add levenshtein fallback |
| **Cohere / Voyage / Mistral providers** | More embedding + chat options |
| **SSE-based MCP transport** | Complement stdio and HTTP POST with persistent SSE MCP connection |
| **Concurrent approval gates** | Session-scoped `ApprovalGate` instead of one global |
| **CI fixes** | Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` and fix all failures |
| **Tests for patch engine** | No tests for `parse_unified_diff`, `apply_hunks`, `PatchEngine::preview()` |
| **Tests for agent runtime** | No async tests for the agent loop or orchestrator |
| **Task queue UI** | Sidebar tree view or progress notification for running background tasks |
| **Diff inline view** | Side-by-side before/after in VS Code diff editor before confirming apply |
| **Bot auth enforcement** | Messaging bots (Slack, WhatsApp) bypass all account/tier/quota checks |
