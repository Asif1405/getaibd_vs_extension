# Multi-Mode Agent Architecture

## Overview
MCP Universal now supports intelligent multi-mode operation similar to Cursor/Copilot, with automatic mode detection and context-aware memory management.

## Modes

### 1. **Ask Mode** (Q&A)
- **Purpose**: Answer questions, explain code, provide information
- **Max Iterations**: 1 (single response)
- **Tools**: Not required
- **Memory**: Optional
- **Use Cases**:
  - "What is Rust?"
  - "Explain how closures work"
  - "What does this function do?"

### 2. **Plan Mode** (Strategy)
- **Purpose**: Break down tasks, suggest approaches, identify risks
- **Max Iterations**: 10
- **Tools**: Not required
- **Memory**: Not required
- **Output Format**:
  ```
  ## Plan
  - Step 1: ...
  - Step 2: ...
  
  ## Risks
  - Potential issues...
  
  ## Approach
  - Recommended strategy...
  ```
- **Use Cases**:
  - "How should I implement authentication?"
  - "What's the best way to structure this API?"
  - "Plan for adding a feature"

### 3. **Agent Mode** (Execution)
- **Purpose**: Autonomous task execution with workspace tools
- **Max Iterations**: 50
- **Tools**: Required (read/write files, git, shell, browser)
- **Memory**: Recommended
- **Capabilities**:
  - Read and modify files
  - Search codebase
  - Execute git commands
  - Run shell commands (with approval)
  - Browse web pages
- **Use Cases**:
  - "Implement a login system"
  - "Refactor this module"
  - "Add error handling"

### 4. **Debug Mode** (Problem Solving)
- **Purpose**: Analyze errors, identify root causes, apply fixes
- **Max Iterations**: 20
- **Tools**: Required (read files, search, git diff/log, tests)
- **Memory**: Recommended
- **Workflow**:
  1. **Understand**: Read error messages and logs
  2. **Isolate**: Find failing component
  3. **Hypothesize**: Form theories
  4. **Test**: Verify hypothesis
  5. **Fix**: Apply minimal fix
  6. **Verify**: Check fix works
- **Use Cases**:
  - "There's an error in my code"
  - "Fix this bug"
  - "Debug the failing test"

## Mode Detection

The system automatically detects the appropriate mode based on input keywords:

```rust
// Planning keywords
["plan", "how should i", "what's the best way", "break down", "steps to", 
 "approach for", "strategy", "architecture", "design"]

// Debug keywords
["error", "bug", "crash", "fail", "broken", "debug", "fix", "not working", 
 "doesn't work", "issue with", "problem with", "stack trace", "exception"]

// Task/Agent keywords
["implement", "create", "add", "build", "write", "modify", "update", 
 "refactor", "change", "make", "generate", "can you", "please"]

// Default: Ask mode
```

## API Usage

### Endpoint: `/agent/orchestrated`

**Request**:
```json
{
  "provider": "openai",
  "model": "gpt-4",
  "input": "implement authentication",
  "mode": "agent",  // Optional: "ask", "plan", "agent", "debug"
  "auto_mode": true,  // Enable automatic mode detection
  "use_memory": true  // Enable RAG memory
}
```

**Response (SSE Stream)**:
```
event: mode_selected
data: agent

event: start
data: Agent initialized

event: tool_call
data: read_file({"path": "src/main.rs"})

event: tool_result
data: {...}

event: response
data: I'll implement authentication...

event: complete
data: {"mode": "agent", "result": "...", "iterations": 15}
```

## Memory Management

### Three-Tier Memory System

1. **Short-term**: Current session messages (max 50)
2. **Medium-term**: Recent session embeddings (max 100)
3. **Long-term**: Persistent facts about the project

### Memory Usage by Mode

| Mode   | Short | Medium | Long | Strategy |
|--------|-------|--------|------|----------|
| Ask    | ✓     | ✗      | ✗    | Question context only |
| Plan   | ✓     | ✗      | ✓    | Project structure + current state |
| Agent  | ✓     | ✓      | ✓    | Full context with vector search |
| Debug  | ✓     | ✓      | ✓    | Error context + recent changes |

### Conversation Memory

- **Auto-compression**: When conversation exceeds max_messages (default: 50), older messages are summarized
- **Summary injection**: Compressed context injected as system message
- **Token management**: Keeps recent full messages + older summaries

```rust
ConversationMemory {
    messages: Vec<ToolMessage>,  // Recent full messages
    max_messages: 50,
    summary: Option<String>,  // Compressed older messages
}
```

## Implementation

### Key Files

```
src/agent/
├── modes.rs           # Mode definitions and detection
├── orchestrator.rs    # Mode switching and execution
├── runtime.rs         # Agent execution loop
└── session.rs         # Session state management

src/routes/
└── orchestrated_agent.rs  # HTTP API endpoint

src/memory/
├── store.rs           # Vector + FTS hybrid search
├── embeddings.rs      # Embedding providers
└── persistent.rs      # Long-term fact storage
```

### Usage in VSCode Extension

Update the extension to use the orchestrated endpoint:

```typescript
// vscode-extension/src/client.ts
export function streamOrch orchestratedAgent(
  provider: string,
  model: string,
  input: string,
  autoMode: boolean,
  callbacks: AgentCallbacks,
): AbortController {
  const body = {
    provider,
    model,
    input,
    auto_mode: autoMode,
    use_memory: true,
  };
  
  // Stream SSE events...
}
```

### UI Updates

Add mode indicator to chat panel:

```typescript
// Show detected mode
case "mode_selected":
  displayModeLabel(msg.mode);  // "🧠 Agent Mode"
  break;
```

## Configuration

### Environment Variables

```bash
# Memory system
MCP_MEMORY_ENABLED=true
MCP_MEMORY_EMBEDDING_PROVIDER=openai  # or gemini, ollama
MCP_MEMORY_EMBEDDING_MODEL=text-embedding-3-small

# Auto-mode (default: enabled)
MCP_AUTO_MODE=true

# Max conversation messages before compression
MCP_CONVERSATION_MAX_MESSAGES=50
```

### Config File (config.toml)

```toml
[agent]
auto_mode = true
max_conversation_messages = 50

[memory]
enabled = true
embedding_provider = "openai"
embedding_model = "text-embedding-3-small"
top_k = 10
max_entries = 1000
```

## Benefits

1. **Smart Defaults**: No need to manually select mode
2. **Context-Aware**: Memory system provides relevant information
3. **Token Efficient**: Conversation compression prevents context overflow
4. **Familiar UX**: Similar to Cursor's composer/chat/terminal/debug modes
5. **Extensible**: Easy to add new modes (e.g., "review", "test", "optimize")

## Future Enhancements

- **Multi-mode chaining**: Plan → Agent → Debug workflow
- **Mode confidence scoring**: Show detection confidence
- **Custom mode definitions**: User-defined modes with custom prompts
- **Mode history**: Track mode transitions in session
- **Smart mode switching**: Auto-switch modes mid-conversation
