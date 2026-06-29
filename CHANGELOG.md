# Change Log

All notable changes to the "getaibd" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [0.7.7]

- **Confirm a path exists before touching it.** Added guidance so the agent verifies a
  file/directory actually exists (via a real grep/listing signal or a quick `ls`/`stat`)
  before it reads, lists, patches, moves, or deletes it — and, when a path is missing,
  greps for the real name or lists the parent instead of retrying blindly. Cuts down on
  wasted "file not found" round-trips from guessed paths.

## [0.7.6]

- **Faster, more efficient command use.** The agent now gets explicit guidance to pick
  the quickest command for the job: search with `rg` (not `grep -r`/`find`), read only
  the lines it needs, scope tests/builds to what changed (full suite once at the end),
  reuse earlier output instead of re-running, and never block on long-running processes.
  It's also reminded that commands run without a shell (no pipes, redirects, `&&`, or
  globs), which avoids a common class of failed commands.

## [0.7.5]

- **New `web_search` tool — the agent can look things up online.** It can now search
  the web for the latest package/library versions, API docs, changelogs, and error
  messages instead of digging through your vendored dependencies. Guidance steers it
  to `web_search` rather than reading `.venv`, `node_modules`, or `site-packages`.
  Searches use your GetAIBD account (paid plans) and cost a small per-search fee.
- **Web search works in chat-driven agent runs too.** The orchestrated path now
  registers the same on-demand tools as the basic agent, so semantic codebase search
  and web search are available everywhere.

## [0.7.4]

- **Engine no longer gets killed mid-run.** A slow provider check on `/health` could
  make the liveness probe time out, after which the extension killed the *live* engine
  in the middle of a task. `/health` is now an instant local check (provider
  connectivity moved behind `?providers=1`), a healthy engine is never force-killed,
  and a window reload / second window now **re-attaches** to the running engine
  instead of restarting it. If the connection does drop mid-turn you'll see "Engine
  reconnecting…" and the turn is retried once automatically.
- **Semantic codebase search + memory.** A `semantic_search` tool plus incremental
  startup indexing let the agent find code by meaning, not just exact text.
- **Reusable terminals + `read_terminal`.** Commands run in a persistent terminal pool;
  long-running processes (dev servers, watchers) are left running while the agent keeps
  working, and it can read earlier terminal output on demand.
- **Editable queued messages.** You can edit (or remove) a queued chat message before
  it's sent.
- **Snappier completion.** Background reflection/indexing no longer delays the "done"
  state — the run finishes as soon as the answer is ready.
- **Smarter code search & environment handling.** The agent greps with broader patterns
  (synonyms/antonyms/naming variants) to find the right code, and checks your `.env` /
  compose / Makefile files when a command fails with an environment error.

## [0.7.0]

- **Fix the panel hanging on "Getting ready…" / dead webview.** A stray raw
  newline inside a generated webview string literal (`createTextNode("\n\n")`)
  made the *entire* inline webview script fail to parse, so nothing ran — the boot
  overlay never cleared and attach/send/drag-drop were all dead. Now emitted as a
  valid `"\n\n"`.
- **Drag-and-drop restored and widened to the whole panel.** Dropping an image or
  file anywhere in the panel (not just the small composer box) attaches it; OS/
  Finder drops and VS Code Explorer drags are both handled.
- **Clear guidance for image-incapable models.** Attaching an image and sending to
  a text-only model (e.g. DeepSeek) used to fail with an opaque provider `404`.
  The extension now detects models without image input and asks you to remove the
  attachment or pick a vision-capable model — your staged images are kept.

## [0.6.14]

- **Drag-and-drop now works across the whole panel.** With the webview script
  finally executing (see 0.6.13), drag-and-drop is restored and the drop zone is
  widened from the small composer box to the entire panel — dropping an image or
  file anywhere (including over the message list) now attaches it. OS/Finder drops
  (real `File` objects) and VS Code Explorer drags (`text/uri-list` forwarded to
  the extension host) are both handled.

## [0.6.13]

- **Real root-cause fix for the panel stuck on "Getting ready…".** The webview's
  entire inline `<script>` was failing to parse with `Uncaught SyntaxError:
  Invalid or unexpected token` (`index.html:1442`), so *no* webview JS ran at all
  — no message listener, no `ready` post, no boot watchdog — leaving the spinner
  up forever (and image attach / send dead too). Cause: the collapsible "thinking"
  block used `document.createTextNode("\n\n")`, but because the whole page is built
  from an outer template literal, the `\n` escapes were consumed by the *outer*
  template and emitted as **raw newlines inside a JS string literal** in the
  generated webview, which is a hard syntax error. Double-escaped to `"\\n\\n"` so
  the rendered webview gets a valid `"\n\n"`. The 0.6.9–0.6.12 boot-overlay
  watchdog work is retained as a safety net, but this parse error was the actual
  blocker.

## [0.6.12]

- **Actually fix the panel stuck on "Getting ready…" (one-time overlay).** The
  0.6.11 watchdog was defeated by a webview reload/re-resolve loop: each fresh
  webview re-ran `onReady`, which re-posted `bootStatus: loading` and *re-armed*
  the watchdog, so it never fired and the spinner stayed up even though the
  engine was healthy. The boot overlay is now strictly **one-time**: the watchdog
  arms once on load and runs to completion (no re-arming), and once the overlay
  clears (engine ready or watchdog) it is never shown again — subsequent
  `loading` posts are ignored, and `onReady` goes straight to `ready` after the
  first boot. Net: a reload loop, a lost `ready` post, or a slow engine can no
  longer trap the panel.

## [0.6.11]

- **Fix panel permanently stuck on "Getting ready…"** — the engine starts and
  becomes healthy extension-side, but the overlay was cleared by a single
  `bootStatus: ready` post. If the Secondary Side Bar webview was swapped or
  momentarily detached while the engine was still starting (the health check can
  take ~15s), that one message was lost and the spinner span forever even though
  the engine was up. The overlay is now **fail-open**: a webview watchdog
  force-clears it after a grace window beyond the engine's own 30s health
  timeout, and the extension re-asserts "ready" whenever the panel becomes
  visible again — so a lost post can no longer trap the panel.

## [0.6.10]

- **Fix extension failing to start on Cursor / VS Code 1.105** — 0.6.9 declared a
  minimum engine of `^1.106.0` (pulled in when the chat view moved to the Secondary
  Side Bar, a 1.106-stable contribution point), so editors on the 1.105 base refused
  to load it ("not compatible with VS Code 1.105.1"). Lowered the required engine
  back to `^1.105.0` — the same floor as the last known-good build — which still
  renders the Secondary Side Bar container on those editors.

## [0.6.9]

- **Fix chat stuck on "Getting ready…"** — the boot overlay was gated on network
  `fetch()` calls (provider list, account status) that had no timeout, so a single
  stalled request froze the panel on the loading spinner indefinitely. The overlay
  now clears as soon as the (already time-bounded) engine start and local restores
  finish; provider/auth loading runs in the background and can no longer block boot.
  The boot-path fetches also got a hard 10s timeout as a backstop.

## [0.6.8]

- **Fix: a down completion reviewer no longer ends runs with "completion check
  unavailable — verify work was finished"** — when the lightweight completion-review
  call fails (e.g. the review model is unavailable for the selected provider), the
  agent has no independent signal, so it now trusts the model's decision to stop
  instead of fabricating outstanding items and bailing. Only a *verified* reviewer can
  force the agent to keep going; an unavailable one no longer traps genuine
  completions (Q&A/analysis answers, prose deliverables, or runs that already changed
  the workspace), and an action task that did nothing still gets the normal nudge to
  actually act.
- **Fix: the agent stops grepping the whole repo for the file you have open** — in
  agent mode the currently-open file was excluded from context unless you @-mentioned
  it, so the agent had to search the repository to rediscover what was already on your
  screen (often your in-progress attempt). The active file is now handed to the agent
  up front (skipped only when you've already @-mentioned it), so it starts there.
- **Fix: model reasoning no longer leaks into the answer** — chain-of-thought (whether
  emitted as inline `<thinking>`/`<think>`/`<plan>`/`<reflection>` tags or in a separate
  `reasoning_content` stream field) is now separated from the reply during streaming and
  shown in a dedicated, collapsible "Thinking" block you can expand and scroll. This is
  robust to tags that span tokens and to malformed/unclosed tags (which previously leaked
  raw reasoning, e.g. a stray `</thinking`).

## [0.6.7]

- **Fix: agent no longer hangs forever mid-task ("Agent working…")** — the HTTP client's
  read timeout only fires when *no bytes* arrive, but a gateway that emits SSE
  keepalive/heartbeat bytes during a stalled generation kept resetting it while the
  model produced no real output, so the run could hang indefinitely. The streaming
  reader now has a delta-level idle watchdog: if no token or tool-call data arrives for
  300s the turn is treated as a stalled stream and retried once (draft discarded) before
  surfacing a clean error — instead of spinning at "Agent working…" with no end.

## [0.6.6]

- **Fix: image attachments no longer fail with HTTP 413** — the local engine used
  Axum's default 2 MB request-body limit, but a base64-encoded image easily exceeds
  that, so attaching one returned "Failed to buffer the request body: length limit
  exceeded". The engine now accepts request bodies up to 64 MB.
- **Fix: drag-and-drop from the VS Code Explorer now works** — Explorer drags carry a
  URI list rather than real file objects, which the composer's drop handler ignored.
  It now reads the dropped URIs on the extension host (images become attachments, text
  files become @context), and OS/Finder file drops keep working as before.

## [0.6.5]

- **Fix: agent no longer loops at the end of long runs** — the stall detector that
  decides when to stop force-continuing required the completion reviewer's
  outstanding-items list to be *byte-identical* between reviews. Because that list is
  free-form text the reviewer re-words every round, the check almost never matched, so
  on long runs the agent would finish the work and then keep re-investigating up to the
  hard cap instead of stopping. Stall detection now triggers on the reliable signals —
  no new substantive work since the last review, or the reviewer reporting *roughly the
  same* gaps (fuzzy token match, robust to re-wording) — so a run ends promptly once it's
  genuinely done or stuck.

## [0.6.2]

- **Fix: agent no longer redoes the same step (even file writes)** — a general loop
  guard now fingerprints each turn and stops the agent when it repeats the identical
  action (re-running the same tool call / re-writing the same file) or re-emits the
  same answer. Previously a repeated write counted as "progress" and reset stall
  detection, so the agent could redo finished work many times. Identical repeats no
  longer count as progress; the agent gets one corrective nudge, then stops cleanly.
- **Broader prose-deliverable detection** — "write/draft/compose a description,
  summary, reply, email, …" is recognized as prose (answered once, not force-continued),
  while requests that target a file or code (`README`, `.py`, docstring, etc.) still
  correctly require tools.

## [0.6.1]

- **Fix: agent no longer repeats a finished answer** — for prose deliverables (a PR/MR
  description, commit message, release notes, etc.) the model's text *is* the result, so the
  agent now accepts it and stops. Previously these tasks were misread as needing file changes,
  so the completion reviewer — which trusts workspace diffs — kept force-continuing and
  re-emitting the same answer over and over.

## [0.6.0]

- **Big token-usage reduction** — large tool outputs (full-file `read_file`, noisy
  `run_command` dumps) are now clipped (head + tail, with a marker) before they enter the
  conversation history, so one big result no longer gets re-sent on every later turn. The
  full output is still shown in the UI.
- **Context compaction on large-window models** — the agent now compacts once a conversation
  crosses an absolute token budget, not just 85% of the model window, so million-token-window
  models (e.g. Gemini) stop quietly resending the whole transcript each turn.
- **Model picker "Auto" search (real fix)** — the free model is now identified by the
  catalog's `free` flag rather than a hardcoded id, so searching "Auto" reliably finds it even
  when the engine serves it under a different name; other model names also display properly.
- **Queue** — pressing Enter on an empty composer releases the first queued message (runs it
  now), so a follow-up Enter starts the next in line.
- Pairs with gateway-side prompt-cache improvements (full static-prefix caching) for further
  per-turn input savings on multi-step runs.

## [0.5.0]

- **Cursor-aligned project instructions** — agent runs load `.getaibd/AGENTS.md`, nested
  `subdir/.getaibd/AGENTS.md` (scoped to your active working directory), glob rules from
  `.getaibd/rules/*.md`, and `.getaibd/MEMORY.md` (fallback: root `MEMORY.md`).
- **User rules** — new setting `getaibd.agent.userRules` injected after conduct, before
  project AGENTS.
- **Skills** — catalog from `.getaibd/skills/*/SKILL.md`; `fetch_skill` tool loads full
  instructions on demand; when your task clearly matches exactly one skill, its body is
  auto-injected for that run.
- **MCP client** — optional `.getaibd/mcp.json` (Cursor-compatible `mcpServers` shape)
  spawns stdio MCP servers per agent session; external tools register as `mcp_{server}_{tool}`
  with approval required by default.
- **Shell-first git (breaking)** — removed `git_add`, `git_commit`, `git_push`, and
  `git_reset` tools; use `run_command` for git mutations (with approval). Read-only
  `git_status`, `git_diff`, and `git_log` remain; `git_status` now returns structured
  branch/staged/unstaged/untracked arrays.
- **Extension** — sends `workspace_cwd` and `user_rules` on agent requests; copies root
  `MEMORY.md` → `.getaibd/MEMORY.md` on first engine start when only the legacy file exists.
- **Built-in baseline guidance** — every agent run now gets a tool-agnostic baseline
  (verify-before-commit, discover-don't-assume, testing discipline, correctness, security,
  VC hygiene). A project's own `.getaibd/AGENTS.md` overrides per `##` section and the
  baseline fills only the aspects it omits — so guidance is always present, never duplicated.
- **Auto-managed `.getaibd/.gitignore`** — when a project uses `.getaibd/`, generated state
  (`MEMORY.md`, caches, indexes, DBs) is ignored automatically while authored config
  (`AGENTS.md`, `rules/`, `skills/`) stays tracked; never clobbers a user-edited file.
  Generated memory now writes to `.getaibd/MEMORY.md` (legacy root `MEMORY.md` still read).
- **Prompt caching (sticky routing)** — agent/chat requests forward a stable
  `cache_session_id` so multi-turn sessions pin to the same upstream, maximizing provider
  prompt-cache hits (e.g. OpenRouter `session_id`); workspace + chat scoped.
- **Model picker** — searching "Auto" now finds the free model; it's matched by its display
  label only, so its underlying engine name no longer surfaces under other searches.
- **Queue** — pressing Enter on an empty composer releases the first queued message (runs it
  now) instead of doing nothing, so a follow-up Enter starts the next in line.

## [0.4.43]

- **Agent scope discipline** — general conduct rules injected every run: follow the request
  literally, no unstated extra steps, corrections override, denied actions are final, stop when
  done, and ask_question options stay in scope.
- **Denial tracking** — user-denied tools/commands are blocked for the rest of the run; two
  denials or a "do nothing" answer stops auto-continue; equivalent git commands share one
  fingerprint (`git add .` == `git_add` with `["."]`).
- **Git tools** — `git_reset` and `git_push`; clearer `git_add`/`git_commit` descriptions
  (staged-only commit, no broad staging unless asked); prefer `git_*` over `run_command`.
- **Completion reviewer** — sees denied actions and stop state so it does not force rejected steps.
- **Shorter wrap-ups** — step-limit summaries capped at ~400 tokens.

## [0.4.42]

- **Fix chat stuck on "Getting ready…"** — webview script no longer crashes on startup
  (changes bar initialized after variable declarations).

## [0.4.41]

- **Stop button spins while a task runs** — animated ring on the red stop square; status
  spinner uses a real element so animation works reliably in the VS Code webview.
- **Fix repeat-after-"ok" loops** — the completion reviewer now judges the **active task**
  from chat history (last real question + progress so far), not the literal word "ok".
  Follow-ups get a continuity prompt; incomplete work resumes from the checkpoint instead
  of restarting; tool nudges only fire for action tasks that still need mutations.
- **Fix repeat summaries after git commit/push** — `git commit`/`git push` count as real
  progress; the reviewer treats a clean tree after commit as done; empty "missing" lists
  no longer force another summary loop.
- **No duplicate assistant bubbles** — if a response was already streamed, the final
  `done` event no longer appends a second copy.
- **Changes bar above chat input** — pending file edits show in a Cursor-style bar with
  expandable file list, per-file accept/reject, **Accept all**, **Reject**, and **Review**
  (multi-file diff, PR-style).
- **Editable user messages** — hover a user message and click **✎** to edit and resend;
  rolls back to that turn (reverting later file changes) and runs again with the new text.

## [0.4.40]

- **Free "Auto" model works at zero credits** — paid keys no longer block the free
  model when balance is at the floor; only paid models require topping up.
- **Settings: Get Integration API Key** opens the site integrations page.
- **Top-up prompts include** `https://getaibd.com/dashboard`.
  - **Stop shows a spinner only while cancelling** — red stop square with a spinning ring
    while a task runs; clicking stop swaps to a loader until the run ends.
- **Chat typography** — larger body text, clearer markdown headings/lists/blockquotes,
  bordered code blocks, and a more visible user-message background.
- **Message queue** — stack follow-up prompts while a run is active; reorder (drag or
  arrows), remove, or force-send any queued item to interrupt the current task.

## [0.4.39]

- **RAG toggle in chat header** — `RAG Off` / `RAG On` beside Normal/Reduced cost; preference
  is saved and enables local codebase memory (`use_memory`) on agent runs.

## [0.4.38]

- **Agent mode sends less context by default** — no automatic open-file injection (agent uses
  `read_file` instead), no replay of manual attachments, and local RAG memory off unless
  you enable `getaibd.agent.useMemory`.
- **@mentions in agent** only when you type `@file` in the prompt (explicit opt-in).
- **Tighter caps** — file snippets 4k chars (was 8k), chat history budget 24k chars (was 60k).
- **Reduced cost mode on by default** for GetAIBD (`getaibd.chat.compressDefault`).
- **Open-file attachment off by default** (`getaibd.fileContext.enabled`); enable for Ask/simple chat.
- **Duplicate user message fix** — current turn no longer sent twice in agent history.
- **Reasoning effort control** — Off / Low / Med / High chips in the composer for thinking
  models (default **Med**, was hardcoded High). Lower effort = fewer billed reasoning tokens.
- **Context estimate badge** — shows approximate client payload size while the agent runs.

## [0.4.37]

- **Developer API requires a subscribed package** — paid API keys without a plan see
  models locked in the picker and get a subscribe prompt; balance endpoint returns
  `has_plan`.
- **Credit balance refreshes after every GetAIBD agent/chat turn** so the status bar
  stays accurate instead of showing a stale balance mid-session.
- **402 errors now prompt "Top Up Credits"** when you already have a paid API key
  (instead of wrongly asking for a new key). Free-tier users still get the add-key flow.
- **Low-balance warning** in the status bar when credits fall to the platform floor (≤10).
- **Engine surfaces the full gateway error body** on 402/403 (e.g. balance vs free-tier).

## [0.4.36]

- **Move the Normal | Reduced cost toggle to the chat header** (beside Settings) so the
  composer row stays compact. Labels shortened to Normal / Reduced with tooltips unchanged.

## [0.4.35]

- **Reduced cost mode for GetAIBD agent runs.** A Normal | Reduced cost switch in the
  chat composer (GetAIBD provider only) sends `compress: true` on tool-loop API calls so
  the platform can apply server-side Headroom compression on tool outputs and save credits.
  Preference is persisted across sessions. Default is Normal.

## [0.4.34]

A major agent-reliability pass. Verified end-to-end against every catalog model
(same task to each through the real agent loop): 22/24 now complete the task in
full — the two misses were a slow model hitting the test's own time cap and a
transient upstream provider error, not the agent.

- **Weak models now actually run tools instead of describing them.** When a model
  replies with prose like "[update_plan(…)]" instead of a real tool call, the next
  turn is sent with `tool_choice=required`, forcing it to act. This fixes models
  (e.g. Llama 4 Maverick) that previously narrated file creation and produced
  nothing.
- **The completion reviewer now checks the real workspace, not the chat.** It is
  given the actual `git` changes, so "I created USER_MANUAL.md" with no file on
  disk is caught and the agent is sent back to finish — instead of quitting before
  the files are written.
- **No more premature stops; runs are governed by the context budget.** Instead of
  a fixed step ceiling, the agent keeps working until the task is verified done or
  it is genuinely stuck (no new progress across several reviews), summarizing at
  85% of the model's real window to make room. Safety backstops prevent runaways.
- **A persistent task plan.** The agent keeps a checklist via a new `update_plan`
  tool that is pinned in context and never summarized away, so it doesn't lose its
  place on long tasks.
- **Per-model real context windows** are now read from the catalog instead of
  guessed, so summarization triggers at the right time for every model.
- **Provider outages surface as errors instead of silent "done."** A mid-stream
  upstream error (e.g. credit exhaustion) used to look like an empty turn and make
  the agent stop as if finished; it now reports the real error. Relatedly, when the
  completion check itself can't run, the agent never declares success unless real
  work actually happened.
- **Fixed strict-provider tool rejections** (e.g. Groq) by accepting and coercing
  boolean arguments sent as strings.
- The completion-review model is now overridable via `GETAIBD_COMPLETION_MODEL`.

## [0.4.33]

- **Fixed the agent failing unpredictably (e.g. "no file was created") on many
  models.** The engine estimated the context window per model, but unknown model
  families (Kimi, Llama 4 Maverick/Scout, GPT‑5, Qwen, GLM, …) fell through to a
  tiny 8k default. With such a small budget the agent trimmed — and *dropped* —
  its own tool results and the original task almost immediately, so it lost its
  place, looped, and finished without doing the work. This is why it "sometimes
  worked": models the table happened to know (Gemini 1M, Claude 200k) were fine,
  the rest were not. Windows are now correct (128k–1M) and unknown models default
  to 128k.
- **New context architecture: summarize, never drop.** Instead of hard-trimming
  old turns once a token budget is hit, the agent now keeps a running token count
  and, only when the conversation crosses 85% of the model's real window,
  summarizes the older middle into a compact note — always preserving the system
  prompt, the original task, and the most recent turns verbatim. A coding agent's
  earlier steps are load-bearing context, so they're folded into a summary rather
  than silently deleted.

## [0.4.32]

- **No more duplicate "done" summaries, and the agent stops sooner.** When the
  completion reviewer decided more work was needed, the engine sent its
  "reviewing…" status update *before* the "discard the draft" signal. The webview
  had already let go of the streamed summary by then, so it couldn't remove it —
  leaving the old summary stacked above the new one. The discard signal is now
  sent first (with no status update in front of it), and the webview also keeps a
  dedicated handle to the last streamed draft that survives status updates, so the
  superseded summary is always removed cleanly.
- **Faster, more decisive completion checks.** A genuinely finished, tool-free
  summary used to be "nudged" up to six times before the reviewer even ran; the
  reviewer now runs immediately once real work has been done. The reviewer was
  also treating the agent's own offers ("Next steps: I can also…") as unfinished
  requirements and re-opening completed tasks — it now judges only the original
  task's explicit requirements and ignores the agent's suggestions, so finished
  work isn't dragged back into more rounds.

## [0.4.31]

- **Cheaper task-completion checks.** The agent runs a strict "is the task fully
  done?" review before finishing (and before each forced continuation). That
  review is a tiny JSON classification, so on GetAIBD it now runs on a fast,
  inexpensive model (`gemini-3.5-flash`) instead of the (possibly expensive)
  model you picked for the work itself — same behavior, lower cost. Bring-your-
  own-key providers are unchanged and keep using your selected model.

## [0.4.30]

- **Fixed the agent looping and stacking repeated summaries.** The progress guard
  that decides whether to auto-continue counted *every* tool call — including
  read-only verification calls like `read_file`/`git_status` — so a model that
  kept re-reading and re-summarizing looked like it was "making progress" and the
  loop never settled. The guard now only counts *mutating* tools (`write_file`,
  `patch_file`, `move_file`, `delete_file`), so once the actual work is done the
  agent stops cleanly instead of re-emitting the same summary.
- **"Chat only" marker for models without tool support.** Models that can't call
  tools (e.g. Perplexity Sonar) now show a small 💬 symbol with a "Chat only"
  tooltip in the model picker, so it's clear they won't drive agent/tool flows.
  Model capabilities are now passed through from the gateway to the picker.

## [0.4.29]

- **Fixed Gemini agent tasks dying after a single tool call.** Gemini 3.x returns
  a `thought_signature` with every tool call, and Google's API *requires* that
  signature to be sent back on the next request. We were dropping it, so the
  follow-up call failed with `HTTP 400 — "Function call is missing a
  thought_signature"`, and the agent stopped after one tool (e.g. it would list
  the directory, then quit without creating any files). The signature is now
  captured and echoed back, so Gemini chains tool calls and completes tasks
  normally. Other providers are unaffected (they simply don't send the field).

## [0.4.28]

- **Fixed tool calls being silently dropped for some models.** The streamed
  tool-call parser required an `index` field that Google's Gemini shim (and a few
  Anthropic proxies) don't always send, so a valid tool call was thrown away and
  the agent would just *narrate* ("I'll use list_directory…") and finish without
  doing anything. Parsing is now resilient to missing `index`/`id`/`arguments`,
  so tools run reliably across providers.
- **Non-reasoning models now act directly instead of stalling.** Smaller, fast
  models (e.g. Claude Haiku, and flash/mini/fast variants) were handed the same
  heavyweight plan/reflect "thinking" scaffolding as reasoning models, which made
  them spend their turn planning in prose rather than calling tools. They now
  skip that ceremony and just do the work; reasoning models keep it.

## [0.4.27]

- **Reverted the task-breakdown to-do list (0.4.24–0.4.26).** The mandatory
  checklist workflow made several models (Gemini, Claude Haiku, GPT-5.4) burn
  their steps/context on planning and, in some cases, claim a task was done
  without actually writing the file. Removing it restores the previous, reliable
  "just do the work to completion" behavior across all providers while we design a
  lighter-weight approach.

## [0.4.23]

- **Fixed cross-project memory bleed.** Each project now gets its own isolated
  long-term memory store. Previously, projects living under the same parent
  folder could share a single memory database, which let one project's learned
  facts and paths leak into another and occasionally confuse the agent about
  which repository it was working in.
- The agent now **always determines the real project root from the files on
  disk** and trusts that over anything recalled from memory, so it stays anchored
  to the project you actually have open.

## [0.4.22]

- The agent now **finishes cleanly the moment your task is done** — it no longer
  re-states its final summary two or three times at the end of a run. When more
  work is genuinely needed it still continues on its own, but any superseded
  "done" message is removed so you only ever see the final result once.

## [0.4.21]

- Fixed: your **most recent chat could disappear after the extension auto-updated**. Chat sessions and history are now kept in a durable on-disk store that's written synchronously on every change, instead of relying on VS Code's lazily-flushed workspace state (which could lose the latest write when the extension host was torn down for an update). Conversations now survive updates and reloads, and existing chats are migrated over automatically.

## [0.4.20]

- The agent now always ends a task with a clear **summary** of what it did, followed by **proactive next-step suggestions** ("Would you like me to add tests next?", "I can wire this into X…") inviting you to continue — so you're never left at a dead end after a task finishes.

## [0.4.19]

- Every code block in a chat reply now has its own **Copy** button in a small header (with the language label), so you can grab a single snippet without selecting it by hand or copying the whole message.

## [0.4.18]

- The agent now **auto-continues until the task is truly done** instead of stopping early and waiting for a Continue click. When it thinks it's finished, it first **self-checks** its work against your original request, and then a strict **reviewer** (same model) independently confirms completion — if anything is missing, the agent keeps working on its own. Safety rails keep this bounded: it only kicks in after real work, never interrupts plain answers or questions, caps forced continuations, and falls back to the manual Continue button if it ever hits that cap. (Agent and Debug modes; Plan/Ask still yield as before.)

## [0.4.17]

- Long-running model calls are no longer cut off. The engine used to apply a 60s total timeout to each request, which killed any single step whose generation ran longer (heavy reasoning, large outputs). It now uses a connect timeout + an idle/read timeout that resets on every token, so an actively streaming response can take as long as it needs — only a genuinely stalled or dead connection fails.

## [0.4.16]

- Further reduced mid-task stops. The "you replied without calling a tool" nudge budget is now counted per *consecutive* narration and refills whenever the agent actually does work — previously a few narrations anywhere in a run would make it give up the moment it next paused. Bumped the budget and strengthened the agent's "run to completion, don't yield with work remaining" instruction.

## [0.4.15]

- Fixed the agent **forgetting the task** on long runs. Context compression used to summarize the *front* of the conversation — which is where the system instructions and the original task live — so after a couple of "Conversation summarized" steps the agent lost track of what it was doing. Now the system prompt and original task are always pinned, only the older middle is summarized, the recent turns are kept verbatim, and compression kicks in far less often.

## [0.4.14]

- If a run ever does reach the step-limit brake, it no longer dead-ends: you get a **Continue** button that resumes the task from where it stopped (history is preserved).

## [0.4.13]

- The agent no longer stops mid-task on long jobs. Per-mode step limits were raised dramatically (Ask 1→25, Plan 16→100, Debug 30→150, Agent 40→250) so real exploration and multi-step work runs to completion. The cap now only exists as a far-off brake against a runaway loop, not something you should hit.

## [0.4.12]

- Listing links now point to [getaibd.com](https://getaibd.com) instead of a source repository, so the Marketplace page has no dead links.

## [0.4.11]

- After the extension auto-updates (which VS Code does silently), you now get a one-time **"GetAIBD updated to vX.Y.Z"** notice with a **What's new** button that opens the changelog. It fires once per version and never on a fresh install.

## [0.4.10]

- The agent now receives your actual **OS and shell** (detected from VS Code) and generates commands dynamically for that environment — instead of relying on hardcoded per-OS rules. Switch your default terminal to cmd, Git Bash, zsh, etc. and the agent adapts.

## [0.4.9]

- Fixed wrong terminal commands on **Windows**. The agent is now told which OS and shell it's running in, so it stops emitting bash-only syntax (`&&`, heredocs, `sed`/`grep`/multi-line `python -c`) that PowerShell rejects. The extension also chains its `cd` step with a separator the active shell actually accepts (PowerShell `;`, cmd/POSIX `&&`).

## [0.4.8]

- Rebranded the listing and docs: GetAIBD is now described as an **AI coding agent for Bangladeshi developers** (no more "Cursor-style"), billed in BDT to one balance with no markup.

## [0.4.7]

- The agent can now ask you a clarifying question as an **interactive options box** (clickable choices + an "Other…" free-text field, single or multi-select) instead of asking in plain text. Powered by a new `ask_question` tool the model calls when it needs you to decide; your answer is fed straight back so it keeps going.

## [0.4.6]

- Settings are pared down to what you actually use: the GetAIBD API key, a few preferences, and a new **Auto-approved tools** list. The engine/agent/memory/context internals are gone.
- Approval cards now have an **Always allow** button — approve a tool once and it runs without asking next time. Revert any tool from Settings → Auto-approved tools.
- Adding or removing your API key now restarts the engine right away so the new key applies without reloading the window (with a Reload Window fallback if the restart fails). This fixes the "added a key but it still shows free tier" mismatch, since the key status now reflects the real platform credential.
- `.getaibd/` is now also added to existing `.cursorignore`, `.dockerignore`, `.vscodeignore`, and other ignore files a project already uses — not just `.gitignore`.

## [0.4.5]

- Reasoning now defaults to **High** for thinking-capable models, so the agent always thinks hard unless you dial it down with the reasoning pill.

## [0.4.4]

- Long-running commands (dev servers, watchers, log tails) no longer hang the agent: once a process keeps running past startup it is released to the background in its own terminal and the agent continues, instead of waiting for the command to exit.
- Terminal tool rows and approval cards now show the full command line (program + arguments) instead of just the program name (e.g. `uv run manage.py runserver` instead of `uv`).

## [0.4.3]

- Chat sessions and history are now stored per workspace, so each project/window keeps its own conversations instead of sharing one global history.

## [0.4.2]

- `git_diff` now shows a real diff of the agent's edits even in folders that aren't git repositories; `git_status`/`git_log` degrade gracefully and `git_add`/`git_commit` give a clear "not a git repo" message.

## [0.4.1]

- Show a "Getting ready…" spinner overlay until the engine and providers finish loading.

## [Unreleased]

- Initial release