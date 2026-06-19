import { getServerUrl, getBaseUrl, authHeaders } from "./util/config";

export interface AccountStatus {
  free: boolean;
  creditsBalance: number | null;
  daysLeft?: number;
  daysLimit?: number;
}

/** Fetches the GetAIBD balance (or free-tier usage) straight from the platform. */
export async function fetchAccountStatus(apiKey: string): Promise<AccountStatus | null> {
  try {
    const resp = await fetch(`${getBaseUrl()}/balance`, {
      headers: { Authorization: `Bearer ${apiKey}` },
    });
    if (!resp.ok) {
      return null;
    }
    const data = (await resp.json()) as {
      free?: boolean;
      credits_balance?: number;
      days_left?: number;
      days_limit?: number;
    };
    return {
      free: !!data.free,
      creditsBalance: typeof data.credits_balance === "number" ? data.credits_balance : null,
      daysLeft: data.days_left,
      daysLimit: data.days_limit,
    };
  } catch {
    return null;
  }
}

export interface ProviderInfo {
  id: string;
  name: string;
}

export interface ModelInfo {
  id: string;
  name: string;
}

export interface ChatMessage {
  role: string;
  content: string;
}

export interface ChatResponse {
  provider: string;
  model: string;
  content: string;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
  };
}

export async function fetchProviders(): Promise<ProviderInfo[]> {
  const resp = await fetch(`${getServerUrl()}/providers`, { headers: authHeaders() });
  if (!resp.ok) {
    throw new Error(`Failed to fetch providers: ${resp.status}`);
  }
  return resp.json() as Promise<ProviderInfo[]>;
}

export async function fetchModels(providerId: string): Promise<ModelInfo[]> {
  const resp = await fetch(`${getServerUrl()}/providers/${providerId}/models`, { headers: authHeaders() });
  if (!resp.ok) {
    throw new Error(`Failed to fetch models: ${resp.status}`);
  }
  return resp.json() as Promise<ModelInfo[]>;
}

export async function chat(
  provider: string,
  model: string,
  messages: ChatMessage[],
  apiKey?: string,
): Promise<ChatResponse> {
  const body: Record<string, unknown> = { provider, model, messages };
  if (apiKey) {body.api_key = apiKey;}
  const resp = await fetch(`${getServerUrl()}/mcp/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`Chat failed (${resp.status}): ${text}`);
  }
  return resp.json() as Promise<ChatResponse>;
}

export async function testProviderConnection(
  provider: string,
  model: string,
  apiKey?: string,
): Promise<{ ok: boolean; error?: string }> {
  try {
    const body: Record<string, unknown> = {
      provider,
      model,
      messages: [{ role: "user", content: "Say OK" }],
      max_tokens: 5,
    };
    if (apiKey) {body.api_key = apiKey;}
    const resp = await fetch(`${getServerUrl()}/mcp/chat`, {
      method: "POST",
      headers: { "Content-Type": "application/json", ...authHeaders() },
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      const text = await resp.text();
      return { ok: false, error: `${resp.status}: ${text}` };
    }
    return { ok: true };
  } catch (err: unknown) {
    return { ok: false, error: err instanceof Error ? err.message : "Connection failed" };
  }
}

export interface SseCallbacks {
  onToken: (text: string) => void;
  onDone: () => void;
  onError: (message: string) => void;
}

export function streamChat(
  provider: string,
  model: string,
  messages: ChatMessage[],
  callbacks: SseCallbacks,
  apiKey?: string,
): AbortController {
  const controller = new AbortController();

  (async () => {
    try {
      const body: Record<string, unknown> = { provider, model, messages };
      if (apiKey) {body.api_key = apiKey;}
      const resp = await fetch(`${getServerUrl()}/sse/chat`, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(body),
        signal: controller.signal,
      });

      if (!resp.ok) {
        const text = await resp.text();
        callbacks.onError(`HTTP ${resp.status}: ${text}`);
        return;
      }

      const reader = resp.body?.getReader();
      if (!reader) {
        callbacks.onError("No response body");
        return;
      }

      const decoder = new TextDecoder();
      let buffer = "";
      let currentEvent = "";
      let currentData = "";

      for (;;) {
        const { done, value } = await reader.read();
        if (done) {break;}

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";

        for (const raw of lines) {
          const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;

          if (line === "") {
            if (currentData) {
              processSseEvent(currentEvent, currentData, callbacks);
              if (currentEvent === "done" || currentEvent === "error") {return;}
            }
            currentEvent = "";
            currentData = "";
            continue;
          }

          if (line.startsWith("event:")) {
            currentEvent = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            const chunk = line.charAt(5) === " " ? line.slice(6) : line.slice(5);
            currentData = currentData ? `${currentData}\n${chunk}` : chunk;
          }
        }
      }

      if (currentData) {
        processSseEvent(currentEvent, currentData, callbacks);
      }
      callbacks.onDone();
    } catch (err: unknown) {
      if (err instanceof Error && err.name === "AbortError") {return;}
      callbacks.onError(err instanceof Error ? err.message : "Stream failed");
    }
  })();

  return controller;
}

function processSseEvent(event: string, data: string, callbacks: SseCallbacks) {
  try {
    const parsed = JSON.parse(data);
    switch (event) {
      case "token":
        if (parsed.content) {callbacks.onToken(parsed.content);}
        break;
      case "done":
        callbacks.onDone();
        break;
      case "error":
        callbacks.onError(parsed.message ?? "Unknown error");
        break;
    }
  } catch {}
}

export interface AgentCallbacks {
  onToolCall: (name: string, args: Record<string, unknown>) => void;
  onToolResult: (name: string, result: unknown) => void;
  onApprovalRequired?: (requestId: string, sessionId: string | undefined, toolName: string, args: Record<string, unknown>) => void;
  onTerminalExec?: (requestId: string, sessionId: string | undefined, args: Record<string, unknown>) => void;
  onText: (text: string) => void;
  onDone: (content: string) => void;
  onComplete: (iterations: number) => void;
  onError: (message: string) => void;
  onPlanning?: (content: string) => void;
  onThinking?: (content: string) => void;
  onReflecting?: (content: string) => void;
  onReplanning?: (content: string) => void;
  onContextCompressed?: (content: string) => void;
  onFileEdit?: (edit: FileEdit) => void;
}

export interface FileEdit {
  path: string;
  old_content?: string;
  new_content?: string;
  too_large?: boolean;
}

export async function sendApproval(requestId: string, approved: boolean, sessionId?: string): Promise<void> {
  await fetch(`${getServerUrl()}/agent/approve`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify({ request_id: requestId, approved, session_id: sessionId }),
  });
}

/** Posts a delegated terminal command's captured result back to the engine. */
export async function sendTerminalResult(
  requestId: string,
  result: { stdout: string; stderr: string; exit_code: number },
  sessionId?: string,
): Promise<void> {
  await fetch(`${getServerUrl()}/agent/terminal_result`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify({ request_id: requestId, session_id: sessionId, result: JSON.stringify(result) }),
  });
}

export function streamAgent(
  provider: string,
  model: string,
  task: string,
  callbacks: AgentCallbacks,
  options?: { systemPrompt?: string; requireApproval?: boolean; apiKey?: string },
): AbortController {
  const controller = new AbortController();

  (async () => {
    try {
      const body: Record<string, unknown> = {
        provider,
        model,
        task,
        require_approval: options?.requireApproval ?? false,
      };
      if (options?.systemPrompt) {body.system_prompt = options.systemPrompt;}
      if (options?.apiKey) {body.api_key = options.apiKey;}

      const resp = await fetch(`${getServerUrl()}/agent/run`, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(body),
        signal: controller.signal,
      });

      if (!resp.ok) {
        const text = await resp.text();
        callbacks.onError(`HTTP ${resp.status}: ${text}`);
        return;
      }

      const reader = resp.body?.getReader();
      if (!reader) {
        callbacks.onError("No response body");
        return;
      }

      const decoder = new TextDecoder();
      let buffer = "";
      let currentEvent = "";
      let currentData = "";

      for (;;) {
        const { done, value } = await reader.read();
        if (done) {break;}

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";

        for (const raw of lines) {
          const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;

          if (line === "") {
            if (currentData) {
              processAgentEvent(currentEvent, currentData, callbacks);
            }
            currentEvent = "";
            currentData = "";
            continue;
          }

          if (line.startsWith("event:")) {
            currentEvent = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            const chunk = line.charAt(5) === " " ? line.slice(6) : line.slice(5);
            currentData = currentData ? `${currentData}\n${chunk}` : chunk;
          }
        }
      }

      if (currentData) {
        processAgentEvent(currentEvent, currentData, callbacks);
      }
    } catch (err: unknown) {
      if (err instanceof Error && err.name === "AbortError") {return;}
      callbacks.onError(err instanceof Error ? err.message : "Agent stream failed");
    }
  })();

  return controller;
}

function processAgentEvent(
  event: string,
  data: string,
  callbacks: AgentCallbacks,
) {
  try {
    switch (event) {
      case "tool_call": {
        let parsed: { name?: string; arguments?: Record<string, unknown> };
        try {
          parsed = JSON.parse(data);
        } catch {
          parsed = { name: data || "tool", arguments: {} };
        }
        callbacks.onToolCall(parsed.name ?? "tool", parsed.arguments ?? {});
        break;
      }
      case "tool_result": {
        let parsed: { name?: string; result?: unknown };
        try {
          const obj = JSON.parse(data);
          parsed =
            obj && typeof obj === "object" && ("result" in obj || "name" in obj)
              ? obj
              : { result: obj };
        } catch {
          parsed = { result: data };
        }
        callbacks.onToolResult(parsed.name ?? "", parsed.result);
        break;
      }
      case "approval_required": {
        const parsed = JSON.parse(data);
        callbacks.onApprovalRequired?.(parsed.request_id, parsed.session_id, parsed.tool_name, parsed.arguments ?? {});
        break;
      }
      case "terminal_exec": {
        const parsed = JSON.parse(data);
        callbacks.onTerminalExec?.(parsed.request_id, parsed.session_id, parsed.arguments ?? {});
        break;
      }
      case "text":
        callbacks.onText(data);
        break;
      case "done":
        callbacks.onDone(data);
        break;
      case "complete": {
        const parsed = JSON.parse(data);
        callbacks.onComplete(parsed.iterations ?? 0);
        break;
      }
      case "error":
        callbacks.onError(data);
        break;
      case "planning":
        callbacks.onPlanning?.(data);
        break;
      case "thinking":
        callbacks.onThinking?.(data);
        break;
      case "reflecting":
        callbacks.onReflecting?.(data);
        break;
      case "replanning":
        callbacks.onReplanning?.(data);
        break;
      case "context_compressed":
        callbacks.onContextCompressed?.(data);
        break;
      case "file_edit": {
        try {
          callbacks.onFileEdit?.(JSON.parse(data));
        } catch {}
        break;
      }
    }
  } catch {}
}

export interface OrchestratedCallbacks extends AgentCallbacks {
  onModeSelected?: (mode: string) => void;
}

export function streamOrchestrated(
  provider: string,
  model: string,
  input: string,
  mode: string,
  callbacks: OrchestratedCallbacks,
  options?: { apiKey?: string; history?: ChatMessage[]; requireApproval?: boolean; clientTerminal?: boolean },
): AbortController {
  const controller = new AbortController();

  (async () => {
    try {
      const body: Record<string, unknown> = {
        provider,
        model,
        input,
        mode: mode === "auto" ? null : mode,
        auto_mode: mode === "auto",
        use_memory: true,
        history: options?.history ?? [],
        require_approval: options?.requireApproval ?? false,
        client_terminal: options?.clientTerminal ?? false,
      };
      if (options?.apiKey) {body.api_key = options.apiKey;}

      const resp = await fetch(`${getServerUrl()}/agent/orchestrated`, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(body),
        signal: controller.signal,
      });

      if (!resp.ok) {
        const text = await resp.text();
        callbacks.onError(`HTTP ${resp.status}: ${text}`);
        return;
      }

      const reader = resp.body?.getReader();
      if (!reader) {
        callbacks.onError("No response body");
        return;
      }

      const decoder = new TextDecoder();
      let buffer = "";
      let currentEvent = "";
      let currentData = "";

      for (;;) {
        const { done, value } = await reader.read();
        if (done) {break;}

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";

        for (const raw of lines) {
          const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;

          if (line === "") {
            if (currentData) {
              processOrchestratedEvent(currentEvent, currentData, callbacks);
            }
            currentEvent = "";
            currentData = "";
            continue;
          }

          if (line.startsWith("event:")) {
            currentEvent = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            const chunk = line.charAt(5) === " " ? line.slice(6) : line.slice(5);
            currentData = currentData ? `${currentData}\n${chunk}` : chunk;
          }
        }
      }

      if (currentData) {
        processOrchestratedEvent(currentEvent, currentData, callbacks);
      }
    } catch (err: unknown) {
      if (err instanceof Error && err.name === "AbortError") {return;}
      callbacks.onError(err instanceof Error ? err.message : "Orchestrated stream failed");
    }
  })();

  return controller;
}

function processOrchestratedEvent(
  event: string,
  data: string,
  callbacks: OrchestratedCallbacks,
) {
  if (event === "mode_selected") {
    callbacks.onModeSelected?.(data);
    return;
  }
  if (event === "response") {
    callbacks.onText(data);
    return;
  }
  processAgentEvent(event, data, callbacks);
}

export interface PatchPreviewResult {
  plan: string;
  previews: { file: string; operation: string; valid: boolean; error?: string; before_lines?: number; after_lines?: number }[];
  all_valid: boolean;
}

export interface PatchApplyResult {
  patch_id: string;
  plan: string;
  rollback_available: boolean;
  result: { success: boolean; applied: { file: string }[]; failed: { file: string; error?: string }[] };
}

export async function previewPatch(llmResponse: string): Promise<PatchPreviewResult> {
  const resp = await fetch(`${getServerUrl()}/patch/preview`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify({ llm_response: llmResponse }),
  });
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`Patch preview failed (${resp.status}): ${text}`);
  }
  return resp.json() as Promise<PatchPreviewResult>;
}

export async function applyPatch(llmResponse: string): Promise<PatchApplyResult> {
  const resp = await fetch(`${getServerUrl()}/patch/apply`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify({ llm_response: llmResponse }),
  });
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`Patch apply failed (${resp.status}): ${text}`);
  }
  return resp.json() as Promise<PatchApplyResult>;
}

export async function revertPatch(patchId: string): Promise<{ patch_id: string; reverted_files: string[]; errors: string[] }> {
  const resp = await fetch(`${getServerUrl()}/patch/revert/${patchId}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
  });
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`Patch revert failed (${resp.status}): ${text}`);
  }
  return resp.json() as Promise<{ patch_id: string; reverted_files: string[]; errors: string[] }>;
}
