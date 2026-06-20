# Change Log

All notable changes to the "getaibd" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

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