# Branch changes — `backup/wip-agent-semantic-engine` vs `main`

**Base:** `main` @ `cf2d994` (v0.7.0).
**This branch:** 1 commit (`7be659f "Backup WIP: audit fixes, semantic search, agent
runtime, engine resilience"`) **plus uncommitted working-tree changes**.

**Full delta vs `main`:** 70 files, **~5,554 insertions / ~1,088 deletions**.

> Note: this is much larger than just the recent agent/open-file session — most of the
> branch is a security/robustness audit pass (`AUDIT.md`) and a semantic-search + memory
> + agent-runtime overhaul. Sections are grouped by theme; audit items reference the codes
> in `AUDIT.md`.

---

## A. Security & robustness — audit fixes (`AUDIT.md`)

`AUDIT.md` (new, +158) is a full repo audit; the branch implements its fixes:

- **C-1 — command sandbox bypass.** `engine/src/tools/command.rs`: interpreters
  (`python`/`node`/…) may no longer be used with inline-eval flags (`-c`/`-e`/`--eval`/
  `-p`/…); an always-reject list blocks `sh -c '<anything>'`-style bypasses. Documented as
  "approval-gated, not a sandbox."
- **C-2 — `cwd` root escape.** `command.rs`: a model-supplied `cwd` is now routed through
  `workspace::resolve_path`, so an absolute `cwd` (`/`, `/etc`) can no longer escape the
  project root.
- **C-3 — leaked webview/agent resources.** `src/chat/panel.ts`: `dispose()` is now wired
  into `onDidDispose` (aborts in-flight runs, disposes terminals, clears timers).
- **C-4 / H-4 — webview HTML injection + weak nonce.** `src/webview/markdown.ts`: model
  output is sanitized with **DOMPurify** before `innerHTML`; CSP tightened (`img-src`/
  `media-src` no longer allow arbitrary `https:`/`http:`, killing the exfil vector); CSP
  nonce switched to `crypto.randomBytes(16)` (`panel.ts`, `patchPreview.ts`).
- **H-1 — timing-unsafe auth compare.** `engine/src/routes/auth.rs`: constant-time token
  comparison (`constant_time_eq`) with tests.
- **H-2 — Gemini key leaked in URL.** `engine/src/memory/embeddings.rs`: key moved from the
  query string to the `x-goog-api-key` header.
- **H-3 — blocking SQLite on the async runtime.** Heavy memory operations
  (`hybrid_search`, indexing batches, `delete_by_source`, pruning) are now wrapped in
  `tokio::task::spawn_blocking` (`engine/src/tools/semantic.rs`, `runtime.rs`, `store.rs`).
- **H-5 — fire-and-forget engine POSTs.** `src/client.ts`: `sendApproval` /
  `sendTerminalResult` / `sendAskResult` now return a typed `EnginePostResult` and check
  `resp.ok`, so dropped approvals surface instead of silently hanging the run.
- **H-6 — CI didn't run.** Moved the engine workflow from the nested
  `engine/.github/workflows/ci.yml` (which GitHub never executed) to repo-root
  `.github/workflows/ci.yml`.
- **M-3 — untrusted webview messages.** New `src/util/webviewMessage.ts` with
  `msgString` / `msgBool` / `msgNumber` narrowing helpers replacing unchecked `as string`
  casts.
- Related hardening across `state.rs`, `retry.rs`, `circuit_breaker.rs`, `config.rs`,
  `error.rs`, `providers/openai_compat.rs`, `mcp/client.rs`.

## B. Agent runtime overhaul (`engine/src/agent/runtime.rs`, +2,044)

- **Agent mode no longer force-loops.** `orchestrator.rs` + `runtime.rs`: new `action_mode`
  flag on `AgentOptions`; Agent mode runs `auto_complete: false` + `action_mode: true` so it
  stops naturally instead of cycling the completion reviewer + mandatory `update_plan`.
  Debug mode keeps `auto_complete: true`.
- **Stronger task seeding.** `inject_task_seed` rewritten: single-pass lexical seed via
  `task_seed_blocking`, a *conditional* semantic pass (only when the lexical seed is thin),
  comparative-task detection, tighter symbol/phrase limits.
- **Always-on repo map.** Token-budgeted repo outline built in a background
  `spawn_blocking` and injected from turn 1 (never blocks the first model call).
- Supporting changes in `agent/session.rs` (inspection cache), `planner.rs`, `conduct.rs`,
  `thinking.rs`, `modes.rs`.

## C. Open-file / editor-state redesign (extension + engine)

- **Principle:** the open file is ambient IDE state, not an edit target; context is scaled
  to how deliberate the user's gesture is, and the model resolves references (no
  keyword/filename heuristics).
- `src/chat/panel.ts` (`openFileContextForAgent`): `@mention` → full content; **selection**
  → selected snippet + range; **ambient focus** → `[Editor focus: path (line N)]`, no
  content.
- `engine/src/agent/runtime.rs`: `edit_target_hints` (explicit gestures only) replaces
  `open_file_hints`; removed the brittle `open_file_relevant_to_task` /
  `task_points_at_open_file` / `inject_viewport_scope_note` heuristics.
- `engine/src/agent/modes.rs`: system prompt defines EDITOR STATE semantics.

## D. Semantic search + memory

- `engine/src/tools/semantic.rs` (+166, new tool): hybrid vector + keyword ranking, surfaces
  matched file paths to feed task context.
- `engine/src/memory/store.rs`: FTS `keyword_search`, embedding-dimension guarding.
- `engine/src/memory/{indexer,mod,watcher,recent_tracker,summarizer,cache,merkle,...}.rs`:
  indexing, file-watch, and project-graph improvements.
- `engine/src/state.rs`: `resolve_memory_db_path` (project-relative DB), clearer memory
  startup logging; `main.rs`: startup indexing wrapped in `run_startup_indexing_safe`.

## E. Engine resilience / lifecycle (the SIGTERM-mid-run fix)

- `engine/src/routes/health.rs`: bare `GET /health` is now a cheap local liveness probe
  (no upstream calls); provider connectivity moved behind `?providers=1`. This was the root
  cause of the engine being SIGTERM'd mid-run (slow `/health` → failed liveness probe →
  `freePort` killed the live engine).
- `src/engine/manager.ts`: `freePort` won't kill a *healthy* listener unless forced;
  `tryRecoverSessionToken` lets reloads/second windows adopt a running engine; cleaner
  restart path.
- `src/client.ts`: `isEngineConnectionError()` classifier; `src/chat/panel.ts`:
  restart-and-retry-once with an "Engine reconnecting…" notice and friendlier
  `formatAgentError`.

## F. Webview / UI

- `src/chat/panel.ts` (+680): thinking-box shows "Thought for Ns" with a live timer,
  collapsible/scrollable, plan/reflection section labels.
- `src/webview/markdown.ts`: DOMPurify integration (see C-4).
- `src/chat/patchPreview.ts`: crypto nonce; `package.json`/`package-lock.json` add the
  `dompurify` dependency and manifest tweaks.

## G. CI / build / tooling

- Root `.github/workflows/ci.yml` (new) running fmt/clippy/test; removed the orphaned
  nested engine workflow.
- `engine/Cargo.toml` / `Cargo.lock`: new deps (e.g. `dompurify` on the TS side; engine
  crates for the above).
- `.gitignore` / `.vscodeignore` updates.
- Test updates: `engine/tests/{test_routes,test_stop_flow,behavioral_models,test_models,
  test_tools_git}.rs`.

---

## Untracked (not in the diff stat, not committed)

- `AUDIT.md` is **tracked** (part of the diff). Untracked files in the tree:
  - `BRANCH_CHANGES.md` (this file)
  - `DEV_STACK.md`
  - `docs/AGENT_COMPARISON.md`
