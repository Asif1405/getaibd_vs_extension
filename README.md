# GetAIBD — AI coding agent for VS Code

GetAIBD brings an AI coding agent right into your editor: chat with your
codebase, make multi-file edits with inline review, and hand off whole tasks to
an autonomous agent — all with a single GetAIBD account, billed in BDT to one
balance.

## Features

- **Chat & agent in the sidebar** — ask questions, get explanations, or let the
  agent plan and carry out changes end to end.
- **Multi-file edits with review** — every change opens in your editor with
  inline Accept/Reject, so you stay in control.
- **Modes** — `agent`, `plan`, `ask`, and `debug`, or `auto` to pick the best
  fit for your prompt.
- **Project-aware** — understands the file you're working on and your project
  for more relevant answers.
- **Quick code actions** — complete a selection, explain code, or generate
  tests from the editor context menu.
- **One key, one balance** — pay in Taka, use your credits across models.

## Getting started

1. Create an account and subscription at [getaibd.com](https://www.getaibd.com).
2. Generate an API key from the **Integration** dashboard.
3. In VS Code, run **GetAIBD: Set API Key**.
4. Open the agent with **GetAIBD: Open Chat** (`Cmd/Ctrl+Shift+A`).

## Commands

| Command | Description |
| --- | --- |
| `GetAIBD: Open Chat` | Open the chat/agent panel |
| `GetAIBD: Open Agent` | Open the panel in agent mode |
| `GetAIBD: Complete Selection` | Complete the selected code |
| `GetAIBD: Explain Selection` | Explain the selected code |
| `GetAIBD: Generate Tests` | Generate unit tests |
| `GetAIBD: Open Settings` | Open the in-panel settings |
| `GetAIBD: Set API Key` | Store the GetAIBD API key |

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `getaibd.model` | `qwen-flash` | Default model id |
| `getaibd.memory` | `true` | Enable project memory for more relevant context |
| `getaibd.fileContext.enabled` | `false` | Attach open file in **Ask** chat only (agent uses `read_file` to save tokens) |
| `getaibd.agent.useMemory` | `false` | Local RAG in agent runs — or use header **RAG On** (saved per install) |
| `getaibd.chat.compressDefault` | `true` | Start in Reduced cost mode on GetAIBD |
| `getaibd.chat.reasoningDefault` | `medium` | Reasoning effort for thinking models (`off` cheapest, `high` most capable) |
| `getaibd.agent.autoMode` | `auto` | `auto`/`ask`/`plan`/`agent`/`debug` |
| `getaibd.agent.maxIterations` | `25` | Max model calls per agent task |
| `getaibd.agent.userRules` | `""` | Personal rules injected on every agent run (after conduct, before project AGENTS) |

## Project layout (`.getaibd/`)

GetAIBD reads project instructions from a `.getaibd/` folder in your workspace (kept out of
git by default):

```
.getaibd/
  AGENTS.md          # project rules (also nested: packages/api/.getaibd/AGENTS.md)
  MEMORY.md          # long-term facts (legacy root MEMORY.md still works)
  mcp.json           # optional MCP servers for agent sessions
  rules/             # glob-matched rules (*.md with YAML frontmatter)
  skills/            # on-demand skills (*/SKILL.md)
  plans/             # plan output from plan mode
```

Example glob rule (`.getaibd/rules/api.md`):

```markdown
---
description: Backend API conventions
globs: packages/api/**
alwaysApply: false
---
- Use FastAPI dependency injection
```

Example MCP config (`.getaibd/mcp.json`):

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${env:GITHUB_TOKEN}" }
    }
  }
}
```

Git mutations (`git add`, `commit`, `push`, etc.) run via **`run_command`** with approval —
read-only `git_status`, `git_diff`, and `git_log` tools remain available.

## Support

💬 **WhatsApp Support:** [+8801534004464](https://wa.me/8801534004464)
