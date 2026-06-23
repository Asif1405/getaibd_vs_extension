# Change Log

All notable changes to the "getaibd" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

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