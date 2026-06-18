# GetAIBD — Cursor-style AI coding agent for VS Code

GetAIBD is a Cursor-like AI coding agent for VS Code. The extension embeds a
high-performance Rust agent engine (chat, agent loop, multi-mode orchestration,
RAG memory, patch apply/revert, tool calling) and routes **all** inference and
embeddings through your GetAIBD account using a single API key.

This repository is the merge of two projects:

- The Rust agent engine (adapted from `universal-mcp`), kept under [`engine/`](engine/).
- The GetAIBD VS Code extension (the gate: API key, GetAIBD branding, and the
  engine lifecycle), kept under [`src/`](src/).

## Architecture

```
VS Code  ──spawns──▶  engine/ (getaibd-agent, Rust)  ──HTTPS──▶  getaibd.com/v1/api
   src/ (extension)        local 127.0.0.1:39377            (chat + embeddings)
```

- The extension reads `getaibd.apiKey` and spawns the local engine binary with
  `GETAIBD_API_KEY` / `GETAIBD_BASE_URL` set.
- With those env vars present, the engine registers **GetAIBD as the sole
  provider** (chat) and embedding source, and disables the other built-in
  providers — this is the GetAIBD gate.
- The extension talks to the engine over HTTP at `getaibd.serverUrl`
  (default `http://127.0.0.1:39377`).

## Build

Prerequisites: Node.js 20+, Rust (stable), VS Code 1.125+.

```bash
# 1. Build the Rust engine (produces engine/target/release/getaibd-agent)
npm run engine:build

# 2. Build the extension bundle (dist/extension.js)
npm install
npm run compile
```

For packaging, copy the engine binary for the target platform into `bin/`
(`bin/getaibd-agent` or `bin/getaibd-agent.exe`); the extension prefers a
bundled binary, then falls back to `engine/target/{release,debug}` for local
development, then to the `getaibd.enginePath` override.

## Getting started

1. Create an account and subscription at [getaibd.com](https://www.getaibd.com).
2. Generate an API key from the **Integration** dashboard.
3. In VS Code, run **GetAIBD: Set API Key** (or set `getaibd.apiKey` in Settings).
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
| `GetAIBD: Restart Engine` | Restart the local engine |

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `getaibd.apiKey` | `""` | GetAIBD API key (the only required credential) |
| `getaibd.model` | `""` | Default model id |
| `getaibd.baseUrl` | `https://getaibd.com/v1/api` | GetAIBD OpenAI-compatible base URL |
| `getaibd.serverUrl` | `http://127.0.0.1:39377` | Local engine bind address |
| `getaibd.enginePath` | `""` | Optional path to the engine binary |
| `getaibd.agent.autoMode` | `auto` | `auto`/`ask`/`plan`/`agent`/`debug` |

## Support

💬 **WhatsApp Support:** [+8801534004464](https://wa.me/8801534004464)
