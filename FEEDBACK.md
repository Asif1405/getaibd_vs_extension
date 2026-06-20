# GetAIBD System Review

Scope of this review: the VS Code extension (`src/`) plus the bundled Rust engine
(`engine/`, the vendored `mcp-universal` "Universal MCP server"), and how they sit
on top of the `all-in-one-platform` backend (`/v1/api`).

---

## Overall verdict

The engine is genuinely strong and architecturally the right shape for a Cursor-style
coding agent:

- Incremental codebase indexing — `engine/src/memory/` (`merkle.rs`, `call_graph.rs`,
  `embeddings.rs`, `indexer.rs`).
- A real patch engine with preview / apply / revert — `engine/src/patch/`.
- An agent orchestrator with planner, modes, and reflection — `engine/src/agent/`.
- A real MCP stdio module — `engine/src/mcp/`.

Module boundaries are clean. As an *engine*, this is good work.

As a *system*, it has drifted far from the original brief ("use only the API key from
the platform"), and that drift is the source of most of the problems below.

---

## 1. Scope & duplication (biggest issue)

Three layers now do the same job:

- **Platform backend** already owns billing, key pool, routing, quotas
  (`backend/gateway/ai/keypool.py`, `backend/gateway/public_api.py`).
- **Engine** re-implements all of it: its own accounts DB, pricing, tiers, quotas, and
  direct platform provider keys — see `resolve_api_key` / `get_platform_key` in
  `engine/src/routes/chat.rs` (references `openai`/`claude`/`gemini`/`grok`/`deepseek`/…)
  and `engine/src/accounts/`.
- **Extension** adds a "GetAIBD-only lockdown" on top — `src/settings/providerStore.ts`.

These contradict each other: the TS side is locked to a single `getaibd` provider, while
the Rust engine is a universal multi-provider BYOK platform with its own billing.

**Recommendation:** the platform backend should be the sole owner of billing / keys /
routing. The engine should be a thin client that only knows the `getaibd` provider over
`/v1/api`. Strip `engine/src/accounts/`, `pricing`, `tiers`, `platform_keys`, and the
direct provider clients in `engine/src/providers/{openai,claude,gemini,grok,deepseek,…}.rs`.

Also out of scope for a coding agent: `engine/src/messaging/{discord,slack,telegram,whatsapp}.rs`.

---

## 2. Security (treat as critical)

As actually spawned by the extension (`src/engine/manager.ts` passes only
`GETAIBD_API_KEY`, `GETAIBD_BASE_URL`, `MCP_SERVER_HOST/PORT` — **no auth token**), in
`engine/src/main.rs`:

- `state.auth_token` is `None`, so the "Bearer token auth" branch is skipped and the
  `protected` router (`/agent/run`, `/patch/apply`, `/patch/revert`, …) is
  **unauthenticated**.
- CORS is `allow_origin(Any).allow_methods(Any).allow_headers(Any)`.

Combined: **any website the user visits can POST to `http://127.0.0.1:39377/agent/run`
or `/patch/apply` and read or modify files in the workspace**, as can any other local
process. Loopback is not a security boundary here.

**Fix before shipping:** generate a per-spawn random token in the extension, pass it via
env, and require it as a header on every engine endpoint except `/health`; tighten CORS to
a specific origin.

Related smaller smells:

- API key stored in plaintext settings via `getaibd.apiKey` (`src/util/config.ts`) and
  written to Global config in `setApiKey` (`src/extension.ts`). Use VS Code `SecretStorage`
  — `ProviderStore` already has an unused `secrets` path.
- The key is also accepted in request bodies (`api_key` in `src/client.ts`, `byok_request`
  in `resolve_api_key`), so it can leak into logs. The engine already has it from env; don't
  resend it.
- Fixed port 39377 with "reuse if `/health` is healthy" and no identity check lets a
  squatting process capture prompts + workspace content. Use an ephemeral port + handshake,
  or verify identity via the token.

---

## 3. Streaming is partly synthetic

The targeted public contract (`POST /v1/api/generate` in `public_api.py`) is
**non-streaming and text-only**. But the extension does elaborate SSE token streaming
(`streamChat` / `streamAgent` / `streamOrchestrated` in `src/client.ts`). If the engine
routes through `/v1/api/generate`, those tokens can't be real provider streaming; if it
streams by calling providers directly, that's the duplication from #1.

**Recommendation:** expose an OpenAI-compatible streaming endpoint on the gateway (it
already does SSE internally on `/v1/chat`), or be explicit that v1 isn't token-streamed.

---

## 4. Cost governance

`getaibd.agent.maxIterations` defaults to **50**, and each iteration can trigger a
credit-charging `generate` call. With the plan/act/reflect/replan loop that's potentially
dozens of billable calls per task, and the UI surfaces no running credit spend/balance
(the API returns `credits_balance`, but it isn't shown).

**Recommendation:** add a per-task token/credit budget and surface spend in the UI.

---

## 5. Distribution reality check

Shipping a native Rust binary inside a VSIX means: per-OS/arch builds, macOS signing +
notarization (Gatekeeper blocks unsigned spawned binaries), Windows SmartScreen, and a
large VSIX. The resolver (`src/engine/manager.ts`) only falls back to
`engine/target/{release,debug}` (dev) and `bin/` — there is no platform-tagged VSIX or
download-on-demand strategy yet. This will be the hardest part of actually publishing.

---

## 6. Smaller correctness / quality notes

- The SSE parser is duplicated 3x in `src/client.ts`, and multi-line `data:` is overwritten
  (`currentData = line.slice(5)`) instead of accumulated — multi-line payloads break.
- Naming: HTTP routes are called `/mcp/chat`, `MCP_SERVER_PORT`, etc., but those are plain
  REST, not MCP. The real MCP lives in `engine/src/mcp/stdio.rs` and the extension bypasses
  it in favor of a bespoke HTTP protocol. The original goal (a real stdio MCP server any
  host can use) already exists but is unused.
- Vestigial code under the lockdown: `CustomProvider` no-ops, empty `CURATED_MODELS`,
  per-provider secret storage, and inconsistent defaults (`inlineCompletions.provider`
  default is `getaibd` in `package.json` but the snapshot fallback in `providerStore.ts`
  uses `ollama`/`llama3`).

---

## One-line recommendation

The engine is good; the system is over-built. Collapse it to:

```
extension (SecretStorage key + token-authed spawn)
  -> slim engine (local FS / index / patch / agent tools only, single getaibd provider over /v1/api)
    -> platform backend (sole owner of billing / keys / routing, ideally with a streaming endpoint)
```

That matches "use only the API key" and removes the duplicate platform currently being
maintained in two places.

---

## Suggested priority order

1. Security hardening (#2) — blocking for any release.
2. Decide billing/routing ownership and slim the engine (#1).
3. Streaming contract (#3) and cost governance (#4).
4. Distribution/packaging strategy (#5).
5. Cleanup (#6).
