# Change Log

All notable changes to the "getaibd" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [0.4.25]

- **Live task checklist.** For larger tasks the agent now maintains a real,
  structured to-do list (via a new `todo_write` tool) that renders as a checklist
  in the chat — each item shows pending / in-progress / done state and updates as
  the agent works through the task, instead of being plain text it has to retype.

## [0.4.24]

- **Agent now plans big tasks before doing them.** For larger, multi-step
  requests the agent first breaks the work into an ordered to-do checklist,
  posts it, then works through the items one at a time — checking each off as it
  goes — so you can follow progress. Small, one-off requests are still handled
  directly without the extra ceremony.

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