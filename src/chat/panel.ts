import * as vscode from "vscode";
import {
  fetchProviders,
  fetchModels,
  streamChat,
  streamAgent,
  streamOrchestrated,
  sendApproval,
  testProviderConnection,
  type ChatMessage,
} from "../client";
import { scanText, formatWarning } from "../safetype/detector";
import { ensureEngine } from "../engine/manager";
import { PatchPreviewPanel } from "./patchPreview";
import {
  getServerUrl,
  authHeaders,
  getApiKey,
  isFreeToken,
  FREE_MODEL_ID,
  FREE_MODEL_LABEL,
} from "../util/config";
import { ProviderStore, BUILTIN_PROVIDERS, CURATED_MODELS, PROVIDER_META } from "../settings/providerStore";

interface WebviewMessage {
  type: string;
  [key: string]: unknown;
}

interface HistoryEntry {
  kind: "message" | "context";
  role?: string;
  content: string;
  label?: string;
}

interface SessionMeta {
  id: string;
  title: string;
  createdAt: number;
}

const HISTORY_KEY = "getaibd.chatHistory";
const SESSIONS_KEY = "getaibd.sessions";
const ACTIVE_SESSION_KEY = "getaibd.activeSession";
const SESSION_HISTORY_PREFIX = "getaibd.history.";
const PROVIDER_KEY = "getaibd.lastProvider";
const MODEL_KEY = "getaibd.lastModel";
const MODE_KEY = "getaibd.lastMode";
const MAX_RECONNECT = 3;

export class ChatPanel implements vscode.WebviewViewProvider {
  static readonly viewType = "getaibd.chatView";
  private static instance: ChatPanel | undefined;
  private view: vscode.WebviewView | undefined;
  private readonly globalState: vscode.Memento;
  private readonly context: vscode.ExtensionContext;
  private readonly store: ProviderStore;
  private abortController: AbortController | undefined;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  private disposables: vscode.Disposable[] = [];
  private sessions: SessionMeta[] = [];
  private activeSessionId = "";
  private history: HistoryEntry[] = [];
  private currentStreamContent = "";

  constructor(context: vscode.ExtensionContext) {
    this.globalState = context.globalState;
    this.context = context;
    this.store = new ProviderStore(context.globalState, context.secrets);
    this.loadSessions();
    ChatPanel.instance = this;
  }

  /** Registers the chat as a full-height sidebar webview view. */
  static register(context: vscode.ExtensionContext): vscode.Disposable {
    const provider = new ChatPanel(context);
    return vscode.window.registerWebviewViewProvider(ChatPanel.viewType, provider, {
      webviewOptions: { retainContextWhenHidden: true },
    });
  }

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, "media"),
        vscode.Uri.joinPath(this.context.extensionUri, "dist"),
      ],
    };
    view.webview.html = getWebviewContent(view.webview, this.context.extensionUri);
    view.webview.onDidReceiveMessage(
      (msg: WebviewMessage) => this.handleMessage(msg),
      undefined,
      this.disposables,
    );
    view.onDidDispose(() => {
      this.view = undefined;
      this.ready = false;
    }, undefined, this.disposables);
  }

  private post(msg: unknown) {
    this.view?.webview.postMessage(msg);
  }

  static open(context: vscode.ExtensionContext): ChatPanel {
    if (!ChatPanel.instance) {
      ChatPanel.instance = new ChatPanel(context);
    }
    void vscode.commands.executeCommand(`${ChatPanel.viewType}.focus`);
    return ChatPanel.instance;
  }

  static current(): ChatPanel | undefined {
    return ChatPanel.instance;
  }

  private ready = false;
  private pending: Array<() => void> = [];

  /** Runs an action now if the webview is ready, else queues it until it is. */
  private whenReady(fn: () => void) {
    if (this.ready && this.view) {
      fn();
    } else {
      this.pending.push(fn);
    }
  }

  openSettings() {
    this.whenReady(() => this.post({ type: "showSettings" }));
  }

  startNewSession() {
    this.whenReady(() => this.newSession());
  }

  /** Re-reads the stored credential and tells the webview whether it is free-tier. */
  async refreshAuthMode() {
    const key = await getApiKey(this.context.secrets);
    this.post({
      type: "authMode",
      free: isFreeToken(key),
      freeModelId: FREE_MODEL_ID,
      freeModelLabel: FREE_MODEL_LABEL,
    });
  }

  enableAgentMode() {
    this.whenReady(() => this.post({ type: "setAgentMode" }));
  }

  setMode(mode: string) {
    this.whenReady(() => this.post({ type: "setAgentMode", mode }));
  }

  addContext(label: string, code: string) {
    this.history.push({ kind: "context", content: code, label });
    this.saveHistory();
    this.post({ type: "addContext", label, code });
  }

  streamToPanel(provider: string, model: string, messages: ChatMessage[], apiKey?: string) {
    this.streamWithRetry(provider, model, messages, 0, apiKey);
  }

  private streamWithRetry(
    provider: string,
    model: string,
    messages: ChatMessage[],
    attempt: number,
    apiKey?: string,
  ) {
    if (attempt === 0) {
      this.currentStreamContent = "";
      this.post({ type: "streamStart" });
    } else {
      this.currentStreamContent = "";
      this.post({ type: "streamRetry" });
    }

    this.abortController = streamChat(provider, model, messages, {
      onToken: (token) => {
        this.currentStreamContent += token;
        this.post({ type: "streamToken", content: token });
      },
      onDone: () => {
        if (this.currentStreamContent) {
          this.history.push({
            kind: "message",
            role: "assistant",
            content: this.currentStreamContent,
          });
          this.saveHistory();
        }
        this.currentStreamContent = "";
        this.post({ type: "streamEnd" });
        this.abortController = undefined;
      },
      onError: (error) => {
        const isNetworkError = !error.startsWith("HTTP ");
        if (isNetworkError && attempt < MAX_RECONNECT) {
          this.post({
            type: "reconnecting",
            attempt: attempt + 1,
            max: MAX_RECONNECT,
          });
          const delay = Math.min(1000 * 2 ** attempt, 8000);
          this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = undefined;
            this.streamWithRetry(provider, model, messages, attempt + 1, apiKey);
          }, delay);
          return;
        }
        this.currentStreamContent = "";
        this.post({ type: "streamError", error });
        this.abortController = undefined;
        this.maybeHandlePaymentError(error);
      },
    }, apiKey);
  }

  private async handleMessage(msg: WebviewMessage) {
    switch (msg.type) {
      case "ready":
        await this.onReady();
        break;
      case "loadModels":
        if (msg.provider) {await this.loadModels(msg.provider as string);}
        break;
      case "send":
        if (msg.provider && msg.model && msg.text) {
          this.saveSelections(msg.provider as string, msg.model as string);
          await this.sendUserMessage(msg.provider as string, msg.model as string, msg.text as string);
        }
        break;
      case "stop":
        if (this.reconnectTimer) {
          clearTimeout(this.reconnectTimer);
          this.reconnectTimer = undefined;
        }
        this.abortController?.abort();
        this.abortController = undefined;
        this.currentStreamContent = "";
        this.post({ type: "streamEnd" });
        break;
      case "agentSend":
        if (msg.provider && msg.model && msg.text) {
          this.saveSelections(msg.provider as string, msg.model as string);
          await this.sendAgentTask(msg.provider as string, msg.model as string, msg.text as string);
        }
        break;
      case "orchestratedSend":
        if (msg.provider && msg.model && msg.text && msg.mode) {
          this.saveSelections(msg.provider as string, msg.model as string);
          await this.sendOrchestrated(msg.provider as string, msg.model as string, msg.text as string, msg.mode as string);
        }
        break;
      case "clearHistory":
        this.history = [];
        this.saveHistory();
        break;
      case "newSession":
        this.newSession();
        break;
      case "switchSession":
        if (msg.id) {this.switchSession(msg.id as string);}
        break;
      case "deleteSession":
        if (msg.id) {this.deleteSession(msg.id as string);}
        break;
      case "copy":
        if (msg.content) {await vscode.env.clipboard.writeText(msg.content as string);}
        break;
      case "insertToEditor":
        if (msg.content) {
          const editor = vscode.window.activeTextEditor;
          if (editor) {
            await editor.edit((b) => b.insert(editor.selection.active, msg.content as string));
          } else {
            const doc = await vscode.workspace.openTextDocument({ content: msg.content as string });
            await vscode.window.showTextDocument(doc);
          }
        }
        break;
      case "attachFile":
        await this.attachFileContext();
        break;
      case "modeChanged":
        if (msg.mode) {this.globalState.update(MODE_KEY, msg.mode as string);}
        break;
      case "previewPatch":
        if (msg.content) {await this.previewPatch(msg.content as string);}
        break;
      case "needApiKey":
        await this.promptUpgrade();
        break;
      case "refreshModels":
        await this.refreshModels();
        break;
      case "getSettings":
        await this.sendSettings();
        break;
      case "saveProviderConfig":
        await this.handleSaveProvider(msg);
        break;
      case "saveApiKey":
        await this.store.setApiKey(msg.providerId as string, msg.key as string);
        await this.sendSettings();
        break;
      case "removeApiKey":
        await this.store.setApiKey(msg.providerId as string, "");
        await this.sendSettings();
        break;
      case "testProvider":
        await this.handleTestProvider(msg);
        break;
      case "updateServerUrl":
        await vscode.workspace.getConfiguration("getaibd").update("serverUrl", msg.url as string, true);
        break;
      case "updatePreference":
        await this.handleUpdatePreference(msg);
        break;
      case "updateServerConfig":
        await this.handleUpdateServerConfig(msg);
        break;
    }
  }

  private async onReady() {
    try {
      await ensureEngine(this.context);
    } catch {
      /* engine errors are surfaced when the user sends */
    }
    await this.loadProviders();
    this.sendSessions();
    this.restoreHistory();
    this.restoreSelections();
    await this.refreshAuthMode();
    this.ready = true;
    const queued = this.pending.splice(0);
    for (const fn of queued) {
      fn();
    }
  }

  /** Ensures the engine is up, then reloads providers + models (dropdown self-heal). */
  private async refreshModels() {
    try {
      await ensureEngine(this.context);
    } catch {
      /* no credential yet; nothing to load */
    }
    await this.loadProviders();
    await this.loadModels("getaibd");
  }

  /** Detects free-limit / payment-required stream errors and prompts to add a key. */
  private maybeHandlePaymentError(error: string): boolean {
    if (!/\b402\b|Free limit|requires your own/i.test(error)) {
      return false;
    }
    void this.promptUpgrade(error);
    return true;
  }

  /** Free tier only includes the "Auto" model; nudge the user to add a key. */
  private async promptUpgrade(detail?: string) {
    const base = detail && /Free limit/i.test(detail)
      ? "You've used all your free days this month."
      : 'That model needs your own GetAIBD API key. The free tier only includes the "Auto" model.';
    const choice = await vscode.window.showInformationMessage(
      base,
      "Set API Key",
      "Get a Key",
    );
    if (choice === "Set API Key") {
      await vscode.commands.executeCommand("getaibd.setApiKey");
    } else if (choice === "Get a Key") {
      await vscode.env.openExternal(vscode.Uri.parse("https://getaibd.com"));
    }
  }

  private async loadProviders() {
    try {
      const providers = await fetchProviders();
      this.post({ type: "providers", providers });
    } catch {
      this.post({ type: "providers", providers: [] });
    }
    this.post({
      type: "curatedCatalog",
      curatedModels: CURATED_MODELS,
      providerMeta: PROVIDER_META,
      builtinProviders: BUILTIN_PROVIDERS.map(p => ({ id: p.id, label: p.label })),
    });
  }

  private async loadModels(providerId: string) {
    try {
      const models = await fetchModels(providerId);
      this.post({ type: "models", models, provider: providerId });
    } catch {
      this.post({ type: "models", models: [], provider: providerId });
    }
  }

  private saveSelections(provider: string, model: string) {
    this.globalState.update(PROVIDER_KEY, provider);
    this.globalState.update(MODEL_KEY, model);
  }

  private restoreSelections() {
    const config = vscode.workspace.getConfiguration("getaibd");
    const lastProvider = this.globalState.get<string>(PROVIDER_KEY) || config.get<string>("chat.defaultProvider", "getaibd");
    const lastModel = this.globalState.get<string>(MODEL_KEY) || config.get<string>("chat.defaultModel", "");
    let defaultMode = config.get<string>("chat.defaultMode", "agent");
    if (defaultMode === "chat") {defaultMode = "agent";}
    const lastMode = this.globalState.get<string>(MODE_KEY) || defaultMode;

    this.post({
      type: "restoreSelections",
      provider: lastProvider,
      model: lastModel,
      mode: lastMode,
    });
  }

  private async sendSettings() {
    const snapshot = await this.store.getFullSettingsSnapshot();
    let serverConfig = null;
    try {
      const resp = await fetch(`${getServerUrl()}/config`, { headers: authHeaders() });
      if (resp.ok) {
        serverConfig = await resp.json();
      }
    } catch { /* server not reachable */ }
    this.post({ type: "settings", ...snapshot, serverConfig });
  }

  private async handleSaveProvider(msg: WebviewMessage) {
    const id = msg.providerId as string;
    await this.store.saveProviderConfig(id, {
      enabled: msg.enabled as boolean,
      defaultModel: (msg.defaultModel as string) || "",
      url: msg.url as string | undefined,
      endpointId: msg.endpointId as string | undefined,
    });
    await this.sendSettings();
    await this.loadProviders();
  }

  private async handleTestProvider(msg: WebviewMessage) {
    const id = msg.providerId as string;
    const model = (msg.model as string) || "gpt-4o-mini";
    const key = await this.store.getApiKey(id);
    this.post({ type: "testResult", providerId: id, status: "testing" });
    const result = await testProviderConnection(id, model, key);
    this.post({
      type: "testResult",
      providerId: id,
      status: result.ok ? "ok" : "error",
      error: result.error,
    });
  }

  private async handleUpdatePreference(msg: WebviewMessage) {
    const config = vscode.workspace.getConfiguration("getaibd");
    const key = msg.key as string;
    const value = msg.value;
    await config.update(key, value, true);
  }

  private async handleUpdateServerConfig(msg: WebviewMessage) {
    try {
      const resp = await fetch(`${getServerUrl()}/config`, {
        method: "PUT",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(msg.payload),
      });
      const result = await resp.json() as { ok: boolean; message: string };
      if (result.ok) {
        vscode.window.showInformationMessage(result.message);
      } else {
        vscode.window.showErrorMessage(result.message);
      }
    } catch (err: unknown) {
      vscode.window.showErrorMessage(`Failed to update config: ${err instanceof Error ? err.message : String(err)}`);
    }
    await this.sendSettings();
  }

  private async checkSecrets(text: string): Promise<boolean> {
    const detections = scanText(text);
    if (detections.length === 0) {return true;}
    const warning = formatWarning(detections);
    const choice = await vscode.window.showWarningMessage(
      `${warning}\n\nSend anyway?`,
      { modal: true },
      "Send Anyway",
      "Cancel",
    );
    return choice === "Send Anyway";
  }

  private async attachFileContext() {
    const uris = await vscode.window.showOpenDialog({ canSelectMany: true, openLabel: "Attach" });
    if (!uris?.length) {return;}
    for (const uri of uris) {
      const doc = await vscode.workspace.openTextDocument(uri);
      const content = doc.getText();
      const name = vscode.workspace.asRelativePath(uri);
      this.addContext(`@${name}`, content);
    }
  }

  private async resolveAtMentions(text: string): Promise<ChatMessage[]> {
    const mentionRe = /@([^\s]+)/g;
    const messages: ChatMessage[] = [];
    let m: RegExpExecArray | null;
    const resolved = new Set<string>();
    while ((m = mentionRe.exec(text)) !== null) {
      const ref = m[1];
      if (resolved.has(ref)) {continue;}
      resolved.add(ref);
      const files = await vscode.workspace.findFiles(ref, null, 1);
      if (files.length > 0) {
        const doc = await vscode.workspace.openTextDocument(files[0]);
        messages.push({ role: "user", content: `[File: ${ref}]\n\`\`\`\n${doc.getText()}\n\`\`\`` });
        this.post({
          type: "addContext",
          label: `@${ref}`,
          code: doc.getText().slice(0, 500) + (doc.getText().length > 500 ? "\n..." : ""),
        });
      }
    }
    return messages;
  }

  private buildFileContext(): ChatMessage[] {
    const messages: ChatMessage[] = [];
    const config = vscode.workspace.getConfiguration("getaibd");
    if (!config.get<boolean>("fileContext.enabled", true)) {return messages;}
    const editor = vscode.window.activeTextEditor;
    if (editor) {
      const name = vscode.workspace.asRelativePath(editor.document.uri);
      const content = editor.document.getText();
      messages.push({ role: "user", content: `[Currently open file: ${name}]\n\`\`\`\n${content.slice(0, 8000)}\n\`\`\`` });
    }
    return messages;
  }

  private async sendUserMessage(provider: string, model: string, text: string) {
    if (!(await this.checkSecrets(text))) {return;}
    this.history.push({ kind: "message", role: "user", content: text });
    this.saveHistory();
    this.post({ type: "addMessage", role: "user", content: text });

    const apiKey = await this.store.getApiKey(provider);
    const fileCtx = this.buildFileContext();
    const mentionCtx = await this.resolveAtMentions(text);
    const messages: ChatMessage[] = [...fileCtx, ...mentionCtx, { role: "user", content: text }];
    this.streamToPanel(provider, model, messages, apiKey);
  }

  private async sendAgentTask(provider: string, model: string, task: string) {
    if (!(await this.checkSecrets(task))) {return;}
    this.history.push({ kind: "message", role: "user", content: task });
    this.saveHistory();
    this.post({ type: "addMessage", role: "user", content: task });
    this.post({ type: "agentStart" });

    const apiKey = await this.store.getApiKey(provider);
    this.abortController = streamAgent(provider, model, task, {
      onToolCall: (name, args) => {
        this.post({ type: "agentToolCall", name, arguments: args });
      },
      onToolResult: (name, result) => {
        this.post({ type: "agentToolResult", name, result });
      },
      onApprovalRequired: (requestId, toolName, args) => {
        this.handleApprovalRequest(requestId, toolName, args);
      },
      onText: (text) => {
        this.post({ type: "agentText", content: text });
      },
      onPlanning: (content) => {
        this.post({ type: "agentPlanning", content });
      },
      onThinking: (content) => {
        this.post({ type: "agentThinking", content });
      },
      onReflecting: (content) => {
        this.post({ type: "agentReflecting", content });
      },
      onReplanning: (content) => {
        this.post({ type: "agentReplanning", content });
      },
      onContextCompressed: (content) => {
        this.post({ type: "agentContextCompressed", content });
      },
      onDone: (content) => {
        const clean = stripThinking(content);
        if (clean) {
          this.history.push({ kind: "message", role: "assistant", content: clean });
          this.saveHistory();
        }
        this.post({ type: "agentDone", content });
        if (content && looksLikePatch(content)) {
          this.previewPatch(content).catch(() => {});
        }
        const taskId = extractTaskId(content);
        if (taskId) {this.startTaskPolling(taskId);}
      },
      onComplete: (iterations) => {
        this.post({ type: "agentComplete", iterations });
        this.abortController = undefined;
      },
      onError: (error) => {
        this.post({ type: "agentError", error });
        this.abortController = undefined;
        this.maybeHandlePaymentError(error);
      },
    }, { requireApproval: true, apiKey });
  }

  private async sendOrchestrated(provider: string, model: string, text: string, mode: string) {
    if (!(await this.checkSecrets(text))) {return;}
    const priorHistory = this.conversationMessages();
    this.history.push({ kind: "message", role: "user", content: text });
    this.saveHistory();
    this.post({ type: "addMessage", role: "user", content: text });
    this.post({ type: "agentStart" });

    const apiKey = await this.store.getApiKey(provider);
    this.abortController = streamOrchestrated(provider, model, text, mode, {
      onModeSelected: (selectedMode) => {
        this.post({ type: "modeDetected", mode: selectedMode });
      },
      onToolCall: (name, args) => {
        this.post({ type: "agentToolCall", name, arguments: args });
      },
      onToolResult: (name, result) => {
        this.post({ type: "agentToolResult", name, result });
      },
      onApprovalRequired: (requestId, toolName, args) => {
        this.handleApprovalRequest(requestId, toolName, args);
      },
      onText: (text) => {
        this.post({ type: "agentText", content: text });
      },
      onPlanning: (content) => {
        this.post({ type: "agentPlanning", content });
      },
      onThinking: (content) => {
        this.post({ type: "agentThinking", content });
      },
      onReflecting: (content) => {
        this.post({ type: "agentReflecting", content });
      },
      onReplanning: (content) => {
        this.post({ type: "agentReplanning", content });
      },
      onContextCompressed: (content) => {
        this.post({ type: "agentContextCompressed", content });
      },
      onDone: (content) => {
        const clean = stripThinking(content);
        if (clean) {
          this.history.push({ kind: "message", role: "assistant", content: clean });
          this.saveHistory();
        }
        this.post({ type: "agentDone", content });
      },
      onComplete: (iterations) => {
        this.post({ type: "agentComplete", iterations });
        this.abortController = undefined;
      },
      onError: (error) => {
        this.post({ type: "agentError", error });
        this.abortController = undefined;
        this.maybeHandlePaymentError(error);
      },
    }, { apiKey, history: priorHistory });
  }

  private async previewPatch(llmResponse: string): Promise<void> {
    const url = `${getServerUrl()}/patch/preview`;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify({ llm_response: llmResponse }),
      });
      if (!res.ok) {
        const text = await res.text();
        vscode.window.showErrorMessage(`Patch preview failed: ${text}`);
        return;
      }
      const data = (await res.json()) as { plan: string; previews: import("./patchPreview").EditPreview[]; all_valid: boolean };
      PatchPreviewPanel.show({ ...data, llm_response: llmResponse }, this.context);
    } catch (err: unknown) {
      vscode.window.showErrorMessage(`Patch preview error: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  private async handleApprovalRequest(requestId: string, toolName: string, args: Record<string, unknown>) {
    const argsPreview = JSON.stringify(args, null, 2).slice(0, 300);
    const choice = await vscode.window.showWarningMessage(
      `Agent wants to execute: ${toolName}\n\n${argsPreview}`,
      { modal: true },
      "Allow",
      "Deny",
    );
    const approved = choice === "Allow";
    this.post({ type: "approvalStatus", toolName, approved });
    await sendApproval(requestId, approved);
  }

  private startTaskPolling(taskId: string) {
    const pollInterval = setInterval(async () => {
      try {
        const res = await fetch(`${getServerUrl()}/tasks/${taskId}`, { headers: authHeaders() });
        if (!res.ok) { clearInterval(pollInterval); return; }
        const task = (await res.json()) as { status: string; result?: unknown; error?: string };
        this.post({ type: "taskProgress", taskId, task });
        if (task.status !== "running" && task.status !== "queued") {
          clearInterval(pollInterval);
          this.post({ type: "taskComplete", taskId, task });
        }
      } catch {
        clearInterval(pollInterval);
      }
    }, 2000);
  }

  private restoreHistory() {
    for (const entry of this.history) {
      if (entry.kind === "context") {
        this.post({ type: "addContext", label: entry.label, code: entry.content });
      } else {
        this.post({ type: "addMessage", role: entry.role, content: entry.content });
      }
    }
  }

  private saveHistory() {
    this.globalState.update(SESSION_HISTORY_PREFIX + this.activeSessionId, this.history);
    this.maybeTitleFromHistory();
  }

  /** Prior user/assistant turns for this session, capped to keep context affordable. */
  private conversationMessages(): ChatMessage[] {
    const msgs: ChatMessage[] = [];
    for (const e of this.history) {
      if (e.kind !== "message" || !e.content) {continue;}
      if (e.role !== "user" && e.role !== "assistant") {continue;}
      msgs.push({ role: e.role, content: e.content });
    }
    return msgs.slice(-20);
  }

  /** Loads session metadata, migrating any legacy single-history into a first session. */
  private loadSessions() {
    this.sessions = this.globalState.get<SessionMeta[]>(SESSIONS_KEY, []);
    if (this.sessions.length === 0) {
      const legacy = this.globalState.get<HistoryEntry[]>(HISTORY_KEY, []);
      const first: SessionMeta = { id: newId(), title: "New chat", createdAt: Date.now() };
      this.sessions = [first];
      this.activeSessionId = first.id;
      this.globalState.update(SESSIONS_KEY, this.sessions);
      this.globalState.update(ACTIVE_SESSION_KEY, first.id);
      this.globalState.update(SESSION_HISTORY_PREFIX + first.id, legacy);
      if (legacy.length) {
        this.globalState.update(HISTORY_KEY, []);
      }
    } else {
      this.activeSessionId = this.globalState.get<string>(ACTIVE_SESSION_KEY, this.sessions[0].id);
      if (!this.sessions.some((s) => s.id === this.activeSessionId)) {
        this.activeSessionId = this.sessions[0].id;
      }
    }
    this.history = this.globalState.get<HistoryEntry[]>(
      SESSION_HISTORY_PREFIX + this.activeSessionId,
      [],
    );
  }

  private saveSessions() {
    this.globalState.update(SESSIONS_KEY, this.sessions);
    this.globalState.update(ACTIVE_SESSION_KEY, this.activeSessionId);
  }

  private sendSessions() {
    this.post({ type: "sessions", sessions: this.sessions, activeId: this.activeSessionId });
  }

  /** Derives a short session title from the first user message. */
  private maybeTitleFromHistory() {
    const session = this.sessions.find((s) => s.id === this.activeSessionId);
    if (!session || (session.title && session.title !== "New chat")) {
      return;
    }
    const firstUser = this.history.find((e) => e.kind === "message" && e.role === "user");
    if (firstUser) {
      session.title = firstUser.content.replace(/\s+/g, " ").trim().slice(0, 40) || "New chat";
      this.saveSessions();
      this.sendSessions();
    }
  }

  private newSession() {
    this.abortController?.abort();
    this.abortController = undefined;
    const session: SessionMeta = { id: newId(), title: "New chat", createdAt: Date.now() };
    this.sessions.unshift(session);
    this.activeSessionId = session.id;
    this.history = [];
    this.globalState.update(SESSION_HISTORY_PREFIX + session.id, []);
    this.saveSessions();
    this.sendSessions();
    this.post({ type: "clearMessages" });
  }

  private switchSession(id: string) {
    if (id === this.activeSessionId || !this.sessions.some((s) => s.id === id)) {
      return;
    }
    this.abortController?.abort();
    this.abortController = undefined;
    this.activeSessionId = id;
    this.history = this.globalState.get<HistoryEntry[]>(SESSION_HISTORY_PREFIX + id, []);
    this.saveSessions();
    this.sendSessions();
    this.post({ type: "clearMessages" });
    this.restoreHistory();
  }

  private deleteSession(id: string) {
    const idx = this.sessions.findIndex((s) => s.id === id);
    if (idx < 0) {
      return;
    }
    this.sessions.splice(idx, 1);
    this.globalState.update(SESSION_HISTORY_PREFIX + id, undefined);
    if (this.sessions.length === 0) {
      const session: SessionMeta = { id: newId(), title: "New chat", createdAt: Date.now() };
      this.sessions = [session];
    }
    if (id === this.activeSessionId) {
      this.activeSessionId = this.sessions[0].id;
      this.history = this.globalState.get<HistoryEntry[]>(
        SESSION_HISTORY_PREFIX + this.activeSessionId,
        [],
      );
      this.saveSessions();
      this.sendSessions();
      this.post({ type: "clearMessages" });
      this.restoreHistory();
    } else {
      this.saveSessions();
      this.sendSessions();
    }
  }

  private dispose() {
    if (this.reconnectTimer) {clearTimeout(this.reconnectTimer);}
    this.abortController?.abort();
    for (const d of this.disposables) {d.dispose();}
  }
}

function newId(): string {
  return Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}

function looksLikePatch(text: string): boolean {
  return (text.includes('"edits"') || text.includes('"plan"')) && text.includes('"file"') && text.includes('"operation"');
}

function extractTaskId(text: string | undefined): string | null {
  if (!text) {return null;}
  const match = text.match(/"task_id"\s*:\s*"([^"]+)"/);
  return match ? match[1] : null;
}

/** Removes inline reasoning tags so saved/displayed answers stay clean. */
function stripThinking(text: string): string {
  if (!text) {return "";}
  return text
    .replace(/<plan>[\s\S]*?<\/plan>/gi, "")
    .replace(/<thinking>[\s\S]*?<\/thinking>/gi, "")
    .replace(/<reflection>[\s\S]*?<\/reflection>/gi, "")
    .replace(/<\/?(plan|thinking|reflection)>/gi, "")
    .trim();
}

/* ───────────────────────────── WEBVIEW HTML ───────────────────────────── */

function getNonce(): string {
  let text = "";
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (let i = 0; i < 32; i++) {
    text += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return text;
}

function getWebviewContent(webview: vscode.Webview, extensionUri: vscode.Uri): string {
  const nonce = getNonce();
  const cspSource = webview.cspSource;
  const markdownUri = webview.asWebviewUri(
    vscode.Uri.joinPath(extensionUri, "dist", "markdown.js"),
  );
  return /*html*/ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1.0">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${cspSource} https: http: data: blob:; media-src ${cspSource} https: http: data: blob:; font-src ${cspSource} https: data:; style-src 'unsafe-inline' ${cspSource}; script-src 'nonce-${nonce}';">
<script nonce="${nonce}" src="${markdownUri}"></script>
<title>GetAIBD</title>
<style>
:root {
  --bg: var(--vscode-editor-background);
  --fg: var(--vscode-editor-foreground);
  --input-bg: var(--vscode-input-background);
  --input-border: var(--vscode-input-border, #3c3c3c);
  --input-fg: var(--vscode-input-foreground);
  --btn-bg: var(--vscode-button-background);
  --btn-fg: var(--vscode-button-foreground);
  --btn-hover: var(--vscode-button-hoverBackground);
  --border: var(--vscode-panel-border, #2d2d2d);
  --user-bg: var(--vscode-textBlockQuote-background, #2a2a2a);
  --code-bg: var(--vscode-textCodeBlock-background, #1e1e1e);
  --error-fg: var(--vscode-errorForeground, #f44747);
  --muted: var(--vscode-descriptionForeground, #888);
  --warn-bg: var(--vscode-inputValidation-warningBackground, #4d3a00);
  --badge-bg: var(--vscode-badge-background, #4d4d4d);
  --badge-fg: var(--vscode-badge-foreground, #fff);
  --focus: var(--vscode-focusBorder, #007fd4);
  --success: #4ec9b0;
  --list-hover: var(--vscode-list-hoverBackground, #2a2d2e);
}

* { margin: 0; padding: 0; box-sizing: border-box; }

body {
  background: var(--bg);
  color: var(--fg);
  font-family: var(--vscode-font-family, system-ui, -apple-system, sans-serif);
  font-size: 13px;
  display: flex;
  flex-direction: column;
  height: 100vh;
  overflow: hidden;
}

/* ── Header ── */
.header {
  display: flex;
  align-items: center;
  padding: 0 12px;
  height: 40px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
  gap: 2px;
}

.tab {
  padding: 8px 14px;
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
  border: none;
  background: none;
  color: var(--muted);
  border-bottom: 2px solid transparent;
  transition: all 0.15s;
}
.tab:hover { color: var(--fg); }
.tab.active { color: var(--fg); border-bottom-color: var(--btn-bg); }

.header-spacer { flex: 1; }

.sessions-panel {
  flex-shrink: 0;
  max-height: 240px;
  overflow-y: auto;
  border-bottom: 1px solid var(--border);
  background: var(--bg);
}
.session-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 7px 12px;
  cursor: pointer;
  font-size: 12px;
  border-bottom: 1px solid var(--border);
}
.session-item:hover { background: var(--list-hover); }
.session-item.active { background: var(--list-hover); }
.session-item .s-title {
  flex: 1;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.session-item.active .s-title { font-weight: 600; }
.session-item .s-del {
  opacity: 0;
  border: none;
  background: none;
  color: var(--muted);
  cursor: pointer;
  font-size: 13px;
  padding: 0 4px;
}
.session-item:hover .s-del { opacity: 1; }
.session-item .s-del:hover { color: var(--fg); }

.icon-btn {
  background: none;
  border: none;
  color: var(--muted);
  cursor: pointer;
  padding: 6px;
  border-radius: 4px;
  font-size: 15px;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
}
.icon-btn:hover { background: var(--list-hover); color: var(--fg); }
.icon-btn.active { color: var(--btn-bg); }
.upgrade-btn {
  background: var(--btn-bg);
  color: var(--btn-fg);
  border: none;
  border-radius: 12px;
  padding: 3px 10px;
  margin-right: 6px;
  font-size: 11px;
  font-weight: 600;
  cursor: pointer;
}
.upgrade-btn:hover { background: var(--btn-hover); }

/* ── Messages ── */
.messages {
  flex: 1;
  overflow-y: auto;
  padding: 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.message {
  padding: 10px 14px;
  border-radius: 8px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-word;
  position: relative;
  font-size: 13px;
}

.message.user {
  background: var(--user-bg);
  align-self: flex-end;
  max-width: 85%;
  border-bottom-right-radius: 2px;
}

.message.assistant {
  align-self: flex-start;
  max-width: 95%;
  border-bottom-left-radius: 2px;
  padding-right: 60px;
}

.message .role-label {
  font-size: 10px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  color: var(--muted);
  margin-bottom: 4px;
  display: block;
}

.md { white-space: normal; }
.md > *:first-child { margin-top: 0; }
.md > *:last-child { margin-bottom: 0; }
.md p { margin: 0 0 8px; }
.md h1, .md h2, .md h3, .md h4, .md h5, .md h6 {
  margin: 14px 0 6px;
  line-height: 1.3;
  font-weight: 600;
}
.md h1 { font-size: 1.4em; }
.md h2 { font-size: 1.25em; }
.md h3 { font-size: 1.1em; }
.md h4, .md h5, .md h6 { font-size: 1em; }
.md ul, .md ol { margin: 0 0 8px; padding-left: 22px; }
.md li { margin: 2px 0; }
.md li > p { margin: 0; }
.md blockquote {
  margin: 0 0 8px;
  padding: 2px 10px;
  border-left: 3px solid var(--border);
  color: var(--muted);
}
.md a { color: var(--vscode-textLink-foreground); text-decoration: none; }
.md a:hover { text-decoration: underline; }
.md hr { border: none; border-top: 1px solid var(--border); margin: 12px 0; }
.md pre {
  background: var(--code-bg);
  padding: 10px 12px;
  border-radius: 6px;
  overflow-x: auto;
  margin: 0 0 8px;
}
.md pre code { background: none; padding: 0; font-size: 12px; }
.md code {
  background: var(--code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: var(--vscode-editor-font-family, monospace);
  font-size: 12px;
}
.md table {
  border-collapse: collapse;
  margin: 0 0 8px;
  width: auto;
  font-size: 12px;
}
.md th, .md td {
  border: 1px solid var(--border);
  padding: 4px 10px;
  text-align: left;
}
.md th { background: var(--list-hover); font-weight: 600; }
.md img { max-width: 100%; border-radius: 6px; }

.message code {
  background: var(--code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: var(--vscode-editor-font-family, monospace);
  font-size: 12px;
}

.message pre {
  background: var(--code-bg);
  padding: 10px;
  border-radius: 6px;
  overflow-x: auto;
  margin: 6px 0;
}
.message pre code { background: none; padding: 0; }

.msg-action-btn {
  position: absolute;
  top: 6px;
  background: var(--input-bg);
  border: 1px solid var(--border);
  color: var(--muted);
  border-radius: 4px;
  padding: 2px 8px;
  cursor: pointer;
  font-size: 10px;
  opacity: 0;
  transition: opacity 0.15s;
}
.message:hover .msg-action-btn { opacity: 1; }
.msg-action-btn:hover { color: var(--fg); border-color: var(--fg); }
.btn-copy { right: 6px; }
.btn-insert { right: 48px; }

.context-block {
  background: var(--code-bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 12px;
  font-size: 12px;
}
.context-block .label { font-size: 11px; color: var(--muted); margin-bottom: 4px; display: block; }
.context-block pre { margin: 0; font-family: var(--vscode-editor-font-family, monospace); white-space: pre-wrap; font-size: 12px; }

.error-msg { color: var(--error-fg); padding: 8px 14px; font-size: 12px; }

.spinner { display: none; padding: 8px 14px; color: var(--muted); font-size: 12px; align-items: center; gap: 8px; }
.spinner.visible { display: flex; }
.spinner::before { content: ''; width: 14px; height: 14px; border: 2px solid var(--muted); border-top-color: transparent; border-radius: 50%; animation: spin 0.8s linear infinite; flex-shrink: 0; }
@keyframes spin { to { transform: rotate(360deg); } }

.reconnect-banner { display: none; padding: 6px 14px; background: var(--warn-bg); color: var(--fg); font-size: 12px; text-align: center; border-radius: 4px; margin: 0 12px; }
.reconnect-banner.visible { display: block; }

.tool-call { background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; font-size: 12px; margin: 2px 0; }
.tool-call .tool-name { font-weight: 600; color: var(--btn-bg); font-size: 12px; }
.tool-call .tool-args { font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; color: var(--muted); white-space: pre-wrap; max-height: 120px; overflow-y: auto; margin-top: 4px; }
.tool-result { background: var(--code-bg); border-left: 3px solid var(--btn-bg); border-radius: 0 6px 6px 0; padding: 6px 12px; font-size: 11px; font-family: var(--vscode-editor-font-family, monospace); white-space: pre-wrap; max-height: 200px; overflow-y: auto; color: var(--muted); margin: 2px 0; }
.agent-status { padding: 4px 14px; font-size: 11px; color: var(--muted); font-style: italic; }

/* ── Thinking Blocks ── */
.thinking-block {
  margin: 4px 0;
  border-radius: 6px;
  border: 1px solid var(--border);
  overflow: hidden;
  font-size: 12px;
}
.thinking-block summary {
  padding: 6px 12px;
  cursor: pointer;
  font-weight: 500;
  font-size: 11px;
  user-select: none;
  display: flex;
  align-items: center;
  gap: 6px;
}
.thinking-block summary::marker { font-size: 10px; }
.thinking-block .thinking-content {
  padding: 6px 12px 8px;
  font-size: 11px;
  white-space: pre-wrap;
  font-family: var(--vscode-editor-font-family, monospace);
  max-height: 250px;
  overflow-y: auto;
  border-top: 1px solid var(--border);
}
.thinking-block.plan { background: rgba(59, 130, 246, 0.08); }
.thinking-block.plan summary { color: #60a5fa; }
.thinking-block.plan .thinking-content { color: #93bbfc; }
.thinking-block.thinking { background: rgba(148, 163, 184, 0.06); }
.thinking-block.thinking summary { color: var(--muted); }
.thinking-block.thinking .thinking-content { color: var(--muted); }
.thinking-block.reflection { background: rgba(192, 132, 252, 0.08); }
.thinking-block.reflection summary { color: #c084fc; }
.thinking-block.reflection .thinking-content { color: #d8b4fe; }
.thinking-block.replan { background: rgba(251, 191, 36, 0.08); }
.thinking-block.replan summary { color: #fbbf24; }
.thinking-block.replan .thinking-content { color: #fcd34d; }
.context-compressed-msg { padding: 4px 14px; font-size: 11px; color: var(--muted); font-style: italic; display: flex; align-items: center; gap: 4px; }
.context-compressed-msg::before { content: '⟳'; font-size: 12px; }

/* ── Input Bar ── */
.input-bar {
  display: flex;
  align-items: flex-end;
  gap: 6px;
  padding: 10px 12px;
  border-top: 1px solid var(--border);
  flex-shrink: 0;
}

.input-bar textarea {
  flex: 1;
  background: var(--input-bg);
  color: var(--input-fg);
  border: 1px solid var(--input-border);
  border-radius: 8px;
  padding: 9px 12px;
  font-family: inherit;
  font-size: 13px;
  resize: none;
  min-height: 38px;
  max-height: 150px;
  line-height: 1.4;
}
.input-bar textarea:focus { outline: none; border-color: var(--focus); }

.attach-btn {
  background: none;
  border: 1px solid var(--input-border);
  color: var(--muted);
  border-radius: 8px;
  width: 38px;
  height: 38px;
  cursor: pointer;
  font-size: 18px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.attach-btn:hover { color: var(--fg); border-color: var(--fg); }

.model-pill {
  background: var(--input-bg);
  color: var(--fg);
  border: 1px solid var(--input-border);
  border-radius: 8px;
  padding: 0 10px;
  height: 38px;
  cursor: pointer;
  font-size: 12px;
  font-weight: 500;
  white-space: nowrap;
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
  max-width: 210px;
  transition: border-color 0.15s;
}
.model-pill:hover { border-color: var(--focus); }
.model-pill .pill-icon { font-size: 14px; flex-shrink: 0; }
.model-pill .pill-label { overflow: hidden; text-overflow: ellipsis; flex: 1; min-width: 0; }
.model-pill .pill-arrow { font-size: 9px; opacity: 0.6; flex-shrink: 0; }

.send-btn {
  background: var(--btn-bg);
  color: var(--btn-fg);
  border: none;
  border-radius: 8px;
  width: 38px;
  height: 38px;
  cursor: pointer;
  font-size: 16px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.send-btn:hover { background: var(--btn-hover); }
.send-btn.stop { background: var(--error-fg); }

/* ── Cursor-style composer ── */
.brand { font-size: 12px; font-weight: 600; color: var(--muted); letter-spacing: 0.3px; }

.composer {
  margin: 8px 12px 12px;
  border: 1px solid var(--input-border);
  border-radius: 12px;
  background: var(--input-bg);
  padding: 8px 10px 6px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  flex-shrink: 0;
}
.composer:focus-within { border-color: var(--focus); }
.composer textarea {
  width: 100%;
  background: transparent;
  color: var(--input-fg);
  border: none;
  outline: none;
  resize: none;
  font-family: inherit;
  font-size: 13px;
  min-height: 24px;
  max-height: 180px;
  line-height: 1.45;
  padding: 2px 2px 0;
  box-sizing: border-box;
}
.composer-row { display: flex; align-items: center; gap: 6px; }
.composer-spacer { flex: 1; }

.ctl-pill {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  background: var(--vscode-toolbar-hoverBackground, rgba(255,255,255,0.07));
  border: none;
  color: var(--fg);
  border-radius: 14px;
  padding: 4px 10px;
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
  max-width: 220px;
  white-space: nowrap;
}
.ctl-pill:hover { background: var(--list-hover); }
.ctl-pill .ctl-icon { font-size: 13px; flex-shrink: 0; }
.ctl-pill .ctl-arrow { font-size: 9px; opacity: 0.55; flex-shrink: 0; }
.ctl-pill .ctl-label, .ctl-pill .pill-label { overflow: hidden; text-overflow: ellipsis; }
.ctl-pill .pill-icon { display: none; }

.round-btn {
  width: 30px;
  height: 30px;
  border-radius: 50%;
  border: none;
  background: var(--vscode-toolbar-hoverBackground, rgba(255,255,255,0.07));
  color: var(--muted);
  cursor: pointer;
  font-size: 14px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.round-btn:hover { background: var(--list-hover); color: var(--fg); }
.round-btn.send-btn { background: var(--btn-bg); color: var(--btn-fg); }
.round-btn.send-btn:hover { background: var(--btn-hover); }
.round-btn.send-btn.stop { background: var(--error-fg); }

.mode-menu {
  display: none;
  position: fixed;
  bottom: 96px;
  left: 14px;
  min-width: 210px;
  background: var(--vscode-dropdown-background, var(--input-bg));
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 6px;
  z-index: 200;
  box-shadow: 0 -8px 32px rgba(0,0,0,0.45);
}
.mode-menu.open { display: block; }
.mode-item {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 7px 10px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 13px;
}
.mode-item:hover { background: var(--list-hover); }
.mode-item .mi-icon { width: 16px; text-align: center; flex-shrink: 0; }
.mode-item .mi-label { flex: 1; }
.mode-item .mi-check { color: var(--success); visibility: hidden; }
.mode-item.active .mi-check { visibility: visible; }

/* ── Model Dropdown ── */
.model-dropdown {
  display: none;
  position: fixed;
  bottom: 96px;
  left: 8px;
  right: 8px;
  background: var(--vscode-dropdown-background, var(--input-bg));
  border: 1px solid var(--border);
  border-radius: 10px;
  max-height: 440px;
  overflow: hidden;
  z-index: 200;
  box-shadow: 0 -8px 32px rgba(0,0,0,0.45);
  flex-direction: column;
}
.model-dropdown.open { display: flex; }

.model-dropdown .md-header {
  padding: 10px 12px 8px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.model-dropdown .md-header-top {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 8px;
}
.model-dropdown .md-title {
  font-size: 11px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  color: var(--muted);
  flex: 1;
}
.model-dropdown .md-close {
  background: none;
  border: none;
  color: var(--muted);
  cursor: pointer;
  font-size: 16px;
  padding: 2px 4px;
  border-radius: 4px;
  line-height: 1;
}
.model-dropdown .md-close:hover { background: var(--list-hover); color: var(--fg); }

.model-dropdown .search-box input {
  width: 100%;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--input-border);
  border-radius: 6px;
  padding: 7px 10px 7px 30px;
  font-size: 13px;
  outline: none;
  box-sizing: border-box;
}
.model-dropdown .search-box input:focus { border-color: var(--focus); }

/* Provider tabs */
.provider-tabs {
  display: flex;
  gap: 2px;
  overflow-x: auto;
  padding: 6px 8px 0;
  scrollbar-width: none;
  flex-shrink: 0;
  background: var(--vscode-dropdown-background, var(--input-bg));
}
.provider-tabs::-webkit-scrollbar { display: none; }
.provider-tab {
  padding: 5px 10px;
  border-radius: 6px 6px 0 0;
  font-size: 11px;
  font-weight: 500;
  cursor: pointer;
  border: none;
  background: none;
  color: var(--muted);
  white-space: nowrap;
  border-bottom: 2px solid transparent;
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
  transition: color 0.1s;
}
.provider-tab:hover { color: var(--fg); }
.provider-tab.active { color: var(--fg); border-bottom-color: var(--btn-bg); }

.model-dropdown .model-list {
  overflow-y: auto;
  flex: 1;
  padding: 6px 0;
}

.model-group-header {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 8px 14px 4px;
}
.model-group-header .group-icon { font-size: 14px; }
.model-group-header .group-name {
  font-size: 10px;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.7px;
  color: var(--muted);
}

.model-item {
  padding: 7px 14px;
  cursor: pointer;
  font-size: 13px;
  display: flex;
  align-items: center;
  gap: 8px;
  transition: background 0.08s;
}
.model-item:hover { background: var(--list-hover); }
.model-item.selected { background: rgba(0,127,212,0.08); }
.model-item.selected .model-item-name { color: var(--success); font-weight: 500; }
.model-item-check { font-size: 11px; color: var(--success); width: 14px; flex-shrink: 0; }
.model-item-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
.model-item-badges { display: flex; gap: 3px; align-items: center; flex-shrink: 0; }
.ctx-badge {
  font-size: 9px;
  padding: 1px 5px;
  border-radius: 3px;
  background: rgba(255,255,255,0.07);
  color: var(--muted);
  font-family: var(--vscode-editor-font-family, monospace);
}
.tag-badge {
  font-size: 9px;
  padding: 1px 5px;
  border-radius: 3px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.3px;
}
.tag-badge.reasoning { background: rgba(192,132,252,0.15); color: #c084fc; }
.tag-badge.fast      { background: rgba(74,222,128,0.15);  color: #4ade80; }
.tag-badge.powerful  { background: rgba(251,191,36,0.15);  color: #fbbf24; }
.tag-badge.code      { background: rgba(56,189,248,0.15);  color: #38bdf8; }
.tag-badge.open      { background: rgba(148,163,184,0.1);  color: #94a3b8; }
.tag-badge.free      { background: rgba(34,197,94,0.18);   color: #22c55e; }

.model-item.locked { opacity: 0.5; }
.model-item.locked:hover { background: var(--list-hover); }
.model-item-lock { font-size: 10px; opacity: 0.8; }

.model-divider { height: 1px; background: var(--border); margin: 4px 14px; opacity: 0.4; }
.empty-models { padding: 24px 14px; text-align: center; color: var(--muted); font-size: 12px; }

/* ── Settings Panel ── */
.settings-panel {
  display: none;
  flex: 1;
  overflow-y: auto;
  padding: 16px;
}
.settings-panel.visible { display: block; }

.settings-section {
  margin-bottom: 20px;
}

.settings-section h3 {
  font-size: 11px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  color: var(--muted);
  margin-bottom: 10px;
  padding-bottom: 4px;
  border-bottom: 1px solid var(--border);
}

.setting-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}

.setting-row label {
  font-size: 12px;
  min-width: 100px;
  color: var(--fg);
}

.setting-row input[type="text"],
.setting-row input[type="password"],
.setting-row input[type="url"] {
  flex: 1;
  background: var(--input-bg);
  color: var(--input-fg);
  border: 1px solid var(--input-border);
  border-radius: 4px;
  padding: 5px 8px;
  font-size: 12px;
  font-family: inherit;
}
.setting-row input:focus { outline: none; border-color: var(--focus); }

.setting-row input[type="checkbox"] {
  width: 14px;
  height: 14px;
  accent-color: var(--btn-bg);
}

.provider-card {
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
  margin-bottom: 8px;
  transition: border-color 0.15s;
}
.provider-card.enabled { border-color: var(--btn-bg); }

.provider-card-header {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}

.provider-card-header .provider-name {
  font-weight: 600;
  font-size: 13px;
  flex: 1;
}

.provider-card-header .env-hint {
  font-size: 10px;
  color: var(--muted);
  font-family: var(--vscode-editor-font-family, monospace);
}

.provider-card-body {
  display: none;
  flex-direction: column;
  gap: 6px;
  padding-top: 4px;
}
.provider-card.enabled .provider-card-body { display: flex; }

.key-row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.key-row input {
  flex: 1;
  background: var(--input-bg);
  color: var(--input-fg);
  border: 1px solid var(--input-border);
  border-radius: 4px;
  padding: 5px 8px;
  font-size: 12px;
  font-family: var(--vscode-editor-font-family, monospace);
}
.key-row input:focus { outline: none; border-color: var(--focus); }

.small-btn {
  background: var(--input-bg);
  color: var(--muted);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 8px;
  font-size: 11px;
  cursor: pointer;
  white-space: nowrap;
}
.small-btn:hover { color: var(--fg); border-color: var(--fg); }
.small-btn.success { color: var(--success); border-color: var(--success); }
.small-btn.error { color: var(--error-fg); border-color: var(--error-fg); }
.small-btn.primary { background: var(--btn-bg); color: var(--btn-fg); border-color: var(--btn-bg); }
.small-btn.primary:hover { filter: brightness(1.15); }
.small-btn.danger { color: var(--error-fg); }
.small-btn.danger:hover { background: var(--error-fg); color: var(--btn-fg); }

.test-status {
  font-size: 11px;
  margin-left: 4px;
}
.test-status.ok { color: var(--success); }
.test-status.error { color: var(--error-fg); }
.test-status.testing { color: var(--muted); }

.custom-provider-form {
  border: 1px dashed var(--border);
  border-radius: 8px;
  padding: 12px;
  margin-top: 8px;
  display: none;
}
.custom-provider-form.visible { display: block; }

.pref-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
  font-size: 12px;
}
.pref-row label { min-width: 140px; }
</style>
</head>
<body>

<div id="diag" style="display:none;background:#5a1d1d;color:#fff;padding:6px 10px;font-size:11px;white-space:pre-wrap;line-height:1.4"></div>
<div class="header">
  <button class="icon-btn" id="newChatBtn" title="New chat">&#x2795;</button>
  <button class="icon-btn" id="sessionsBtn" title="Chat history">&#x1F551;</button>
  <span class="brand" id="brand">GetAIBD</span>
  <span class="header-spacer"></span>
  <button class="upgrade-btn" id="upgradeBtn" title="Add your API key to unlock all models" style="display:none">&#x1F511; Add API Key</button>
  <button class="icon-btn" id="settingsBtn" title="Settings">&#x2699;</button>
</div>
<div id="sessionsPanel" class="sessions-panel" style="display:none"></div>

<div class="messages" id="messages"></div>
<div class="reconnect-banner" id="reconnectBanner"></div>
<div class="spinner" id="spinner">Generating...</div>

<div class="settings-panel" id="settingsPanel"></div>

<div class="model-dropdown" id="modelDropdown">
  <div class="md-header">
    <div class="md-header-top">
      <span class="md-title">Select Model</span>
      <button class="md-close" id="mdCloseBtn" title="Close">&#x2715;</button>
    </div>
    <div class="search-box">
      <input type="text" id="modelSearch" placeholder="Search models..." autocomplete="off" spellcheck="false" />
    </div>
  </div>
  <div class="provider-tabs" id="providerTabs"></div>
  <div class="model-list" id="modelList"></div>
</div>

<div class="mode-menu" id="modeMenu">
  <div class="mode-item" data-mode="agent"><span class="mi-icon">&#8734;</span><span class="mi-label">Agent</span><span class="mi-check">&#10003;</span></div>
  <div class="mode-item" data-mode="plan"><span class="mi-icon">&#9776;</span><span class="mi-label">Plan</span><span class="mi-check">&#10003;</span></div>
  <div class="mode-item" data-mode="debug"><span class="mi-icon">&#128027;</span><span class="mi-label">Debug</span><span class="mi-check">&#10003;</span></div>
  <div class="mode-item" data-mode="ask"><span class="mi-icon">&#128172;</span><span class="mi-label">Ask</span><span class="mi-check">&#10003;</span></div>
</div>

<div class="composer">
  <textarea id="input" rows="1" placeholder="Ask anything... (use @filename to reference files)"></textarea>
  <div class="composer-row">
    <button class="ctl-pill" id="modePill" title="Mode">
      <span class="ctl-icon" id="modePillIcon">&#8734;</span>
      <span class="ctl-label" id="modePillLabel">Agent</span>
      <span class="ctl-arrow">&#9662;</span>
    </button>
    <button class="ctl-pill model" id="modelPill" title="Model">
      <span class="pill-icon" id="modelPillIcon"></span>
      <span class="pill-label" id="modelPillLabel">Select model</span>
      <span class="ctl-arrow">&#9662;</span>
    </button>
    <span class="composer-spacer"></span>
    <button class="round-btn" id="attachBtn" title="Attach file">&#x1F4CE;</button>
    <button class="round-btn send-btn" id="sendBtn" title="Send">&#9654;</button>
  </div>
</div>

<script nonce="${nonce}">
window.addEventListener("error", function (ev) {
  try {
    var d = document.getElementById("diag");
    if (d) { d.style.display = "block"; d.textContent = "Webview JS error: " + (ev.message || (ev.error && ev.error.message) || "unknown") + "  [" + (ev.filename || "") + ":" + (ev.lineno || "?") + "]"; }
  } catch (_) {}
});
const vscode = acquireVsCodeApi();
const messagesEl = document.getElementById("messages");
const spinnerEl = document.getElementById("spinner");
const reconnectBanner = document.getElementById("reconnectBanner");
const inputEl = document.getElementById("input");
const sendBtn = document.getElementById("sendBtn");
const modelPill = document.getElementById("modelPill");
const modelPillLabel = document.getElementById("modelPillLabel");
const modelPillIcon = document.getElementById("modelPillIcon");
const modePill = document.getElementById("modePill");
const modePillLabel = document.getElementById("modePillLabel");
const modePillIcon = document.getElementById("modePillIcon");
const modeMenu = document.getElementById("modeMenu");
const modelDropdown = document.getElementById("modelDropdown");
const modelSearch = document.getElementById("modelSearch");
const modelList = document.getElementById("modelList");
const providerTabsEl = document.getElementById("providerTabs");
const mdCloseBtn = document.getElementById("mdCloseBtn");
const settingsBtn = document.getElementById("settingsBtn");
const settingsPanel = document.getElementById("settingsPanel");
const newChatBtn = document.getElementById("newChatBtn");
const sessionsBtn = document.getElementById("sessionsBtn");
const sessionsPanel = document.getElementById("sessionsPanel");
const attachBtn = document.getElementById("attachBtn");
const upgradeBtn = document.getElementById("upgradeBtn");
if (upgradeBtn) {
  upgradeBtn.addEventListener("click", () => vscode.postMessage({ type: "needApiKey" }));
}

if (settingsPanel) {
  settingsPanel.addEventListener("click", (e) => {
    const t = e.target.closest("[data-act]");
    if (!t || t.tagName !== "BUTTON") { return; }
    const act = t.getAttribute("data-act");
    const arg = t.getAttribute("data-arg");
    if (act === "closeSettings") { closeSettings(); }
    else if (act === "saveServerUrl") { saveServerUrl(); }
    else if (act === "saveServerCfg") { saveServerCfg(arg); }
    else if (act === "toggleKeyVis") { toggleKeyVis(arg); }
    else if (act === "saveKey") { saveKey(arg); }
    else if (act === "testProvider") { testProvider(arg); }
  });
  settingsPanel.addEventListener("change", (e) => {
    const t = e.target.closest("[data-act]");
    if (!t) { return; }
    const act = t.getAttribute("data-act");
    const arg = t.getAttribute("data-arg");
    if (act === "savePref") { savePref(arg, t.type === "checkbox" ? t.checked : t.value); }
    else if (act === "saveProviderField") { saveProviderField(arg); }
    else if (act === "toggleProvider") { toggleProvider(arg, t.checked); }
  });
}

let streaming = false;
let streamEl = null;
let streamContent = "";
let agentTextEl = null;
let agentTextContent = "";
let thoughtEl = null;
let thoughtBodyEl = null;
let thoughtStart = 0;
let currentMode = "agent";
let settingsOpen = false;

let currentProvider = "";
let currentModel = "";
let allModels = {};      // { providerId: [{id, name}] } from API
let allProviders = [];   // [{id, name}] from server
let curatedModels = {};  // { providerId: [{id, name, ctx, tags}] } from extension
let providerMeta = {};   // { providerId: { icon, color } }
let builtinProviders = []; // [{id, label}]
let activeProviderTab = "all";  // "all" or a provider id

let freeMode = false;
let freeModelId = "qwen-flash";
let freeModelLabel = "Auto";

function modelDisplay(modelId, name) {
  return modelId === freeModelId ? freeModelLabel : (name || modelId);
}

const MODE_PLACEHOLDERS = {
  plan: "Describe what you want to plan...",
  ask: "Ask a question...",
  agent: "Ask anything... (use @filename to reference files)",
  debug: "Describe the error or bug to debug...",
};

const MODE_META = {
  agent: { icon: "\u221E", label: "Agent" },
  plan:  { icon: "\u2630", label: "Plan" },
  debug: { icon: "\uD83D\uDC1B", label: "Debug" },
  ask:   { icon: "\uD83D\uDCAC", label: "Ask" },
};

/* ── Mode switcher (Cursor-style) ── */
function switchMode(mode) {
  currentMode = mode;
  const meta = MODE_META[mode] || MODE_META.agent;
  if (modePillLabel) modePillLabel.textContent = meta.label;
  if (modePillIcon) modePillIcon.textContent = meta.icon;
  document.querySelectorAll(".mode-item").forEach(el => {
    el.classList.toggle("active", el.getAttribute("data-mode") === mode);
  });
  inputEl.placeholder = MODE_PLACEHOLDERS[mode] || MODE_PLACEHOLDERS.agent;
  closeModeMenu();
  vscode.postMessage({ type: "modeChanged", mode });
}

function openModeMenu() {
  closeModeDropdownConflicts();
  modeMenu.classList.add("open");
}
function closeModeMenu() {
  if (modeMenu) modeMenu.classList.remove("open");
}
function closeModeDropdownConflicts() {
  if (modelDropdown) modelDropdown.classList.remove("open");
}

if (modePill) {
  modePill.addEventListener("click", (e) => {
    e.stopPropagation();
    if (modeMenu.classList.contains("open")) closeModeMenu();
    else openModeMenu();
  });
}
document.querySelectorAll(".mode-item").forEach(el => {
  el.addEventListener("click", (e) => {
    e.stopPropagation();
    switchMode(el.getAttribute("data-mode"));
  });
});
document.addEventListener("click", (e) => {
  if (modeMenu && !modeMenu.contains(e.target) && e.target !== modePill && !modePill.contains(e.target)) {
    closeModeMenu();
  }
});

switchMode(currentMode);

/* ── Settings ── */
settingsBtn.addEventListener("click", () => {
  settingsOpen = !settingsOpen;
  if (settingsOpen) {
    settingsBtn.classList.add("active");
    messagesEl.style.display = "none";
    spinnerEl.style.display = "none";
    settingsPanel.classList.add("visible");
    vscode.postMessage({ type: "getSettings" });
  } else {
    closeSettings();
  }
});

function closeSettings() {
  settingsOpen = false;
  settingsBtn.classList.remove("active");
  settingsPanel.classList.remove("visible");
  messagesEl.style.display = "";
}

if (newChatBtn) {
  newChatBtn.addEventListener("click", () => {
    sessionsPanel.style.display = "none";
    vscode.postMessage({ type: "newSession" });
  });
}

if (sessionsBtn) {
  sessionsBtn.addEventListener("click", () => {
    sessionsPanel.style.display = sessionsPanel.style.display === "none" ? "block" : "none";
  });
}

let sessionList = [];
let activeSessionId = "";

function renderSessions() {
  sessionsPanel.innerHTML = "";
  if (!sessionList.length) {
    sessionsPanel.style.display = "none";
    return;
  }
  sessionList.forEach((s) => {
    const item = document.createElement("div");
    item.className = "session-item" + (s.id === activeSessionId ? " active" : "");
    const title = document.createElement("span");
    title.className = "s-title";
    title.textContent = s.title || "New chat";
    item.appendChild(title);
    const del = document.createElement("button");
    del.className = "s-del";
    del.title = "Delete chat";
    del.textContent = "\u2715";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      vscode.postMessage({ type: "deleteSession", id: s.id });
    });
    item.appendChild(del);
    item.addEventListener("click", () => {
      sessionsPanel.style.display = "none";
      vscode.postMessage({ type: "switchSession", id: s.id });
    });
    sessionsPanel.appendChild(item);
  });
}

attachBtn.addEventListener("click", () => vscode.postMessage({ type: "attachFile" }));

/* ── Model Pill / Dropdown ── */
function openModelDropdown() {
  closeModeMenu();
  modelDropdown.classList.add("open");
  modelSearch.value = "";
  activeProviderTab = "all";
  renderProviderTabs();
  renderModelList("");
  const haveModels = Object.values(allModels).some(a => (a || []).length > 0);
  if (!haveModels) {
    vscode.postMessage({ type: "refreshModels" });
  }
  setTimeout(() => modelSearch.focus(), 50);
}

function closeModelDropdown() {
  modelDropdown.classList.remove("open");
}

modelPill.addEventListener("click", (e) => {
  e.stopPropagation();
  if (modelDropdown.classList.contains("open")) {
    closeModelDropdown();
  } else {
    openModelDropdown();
  }
});

if (mdCloseBtn) mdCloseBtn.addEventListener("click", (e) => { e.stopPropagation(); closeModelDropdown(); });

modelSearch.addEventListener("input", () => renderModelList(modelSearch.value.toLowerCase()));
modelSearch.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeModelDropdown();
});

document.addEventListener("click", (e) => {
  if (!modelDropdown.contains(e.target) && e.target !== modelPill) {
    closeModelDropdown();
  }
});

function getProviderIcon(providerId) {
  return (providerMeta[providerId] || {}).icon || "🤖";
}

function getProviderLabel(providerId) {
  const b = builtinProviders.find(p => p.id === providerId);
  if (b) return b.label;
  const ap = allProviders.find(p => p.id === providerId);
  if (ap) return ap.name;
  return providerId;
}

function getAllProviderIds() {
  const ids = new Set();
  for (const id of Object.keys(curatedModels)) ids.add(id);
  for (const p of allProviders) ids.add(p.id);
  for (const p of builtinProviders) ids.add(p.id);
  return [...ids];
}

function renderProviderTabs() {
  if (!providerTabsEl) return;
  providerTabsEl.innerHTML = "";

  const allTab = document.createElement("button");
  allTab.className = "provider-tab" + (activeProviderTab === "all" ? " active" : "");
  allTab.textContent = "All";
  allTab.addEventListener("click", () => { activeProviderTab = "all"; renderProviderTabs(); renderModelList(modelSearch.value.toLowerCase()); });
  providerTabsEl.appendChild(allTab);

  for (const pid of getAllProviderIds()) {
    const hasCurated = (curatedModels[pid] || []).length > 0;
    const hasApi = (allModels[pid] || []).length > 0;
    if (!hasCurated && !hasApi) continue;
    const tab = document.createElement("button");
    tab.className = "provider-tab" + (activeProviderTab === pid ? " active" : "");
    const icon = getProviderIcon(pid);
    const label = getProviderLabel(pid);
    tab.innerHTML = '<span>' + icon + '</span><span>' + escapeHtml(label) + '</span>';
    tab.addEventListener("click", () => { activeProviderTab = pid; renderProviderTabs(); renderModelList(modelSearch.value.toLowerCase()); });
    providerTabsEl.appendChild(tab);
  }
}

function renderModelList(filter) {
  modelList.innerHTML = "";
  let total = 0;

  const providerIds = activeProviderTab === "all" ? getAllProviderIds() : [activeProviderTab];

  for (const pid of providerIds) {
    const curated = (curatedModels[pid] || []).filter(m =>
      !filter ||
      m.id.toLowerCase().includes(filter) ||
      m.name.toLowerCase().includes(filter) ||
      getProviderLabel(pid).toLowerCase().includes(filter)
    );

    const apiModels = (allModels[pid] || []).filter(m =>
      !curated.find(c => c.id === m.id) &&
      (!filter || m.id.toLowerCase().includes(filter) || (m.name || "").toLowerCase().includes(filter))
    );

    if (curated.length === 0 && apiModels.length === 0) continue;

    const header = document.createElement("div");
    header.className = "model-group-header";
    const meta = providerMeta[pid] || {};
    header.innerHTML = '<span class="group-icon">' + (meta.icon || "🤖") + '</span>'
      + '<span class="group-name">' + escapeHtml(getProviderLabel(pid)) + '</span>';
    modelList.appendChild(header);

    for (const m of curated) {
      modelList.appendChild(makeModelItem(pid, m.id, m.name, m.ctx, m.tags || []));
      total++;
    }

    if (apiModels.length > 0) {
      if (curated.length > 0) {
        const div = document.createElement("div");
        div.className = "model-divider";
        modelList.appendChild(div);
      }
      for (const m of apiModels) {
        modelList.appendChild(makeModelItem(pid, m.id, m.name || m.id, undefined, []));
        total++;
      }
    }
  }

  if (total === 0) {
    const empty = document.createElement("div");
    empty.className = "empty-models";
    empty.textContent = filter ? ('No models match "' + filter + '"') : "Loading models… (starting engine — pick Set API Key or Use Free if prompted)";
    modelList.appendChild(empty);
  }
}

function makeModelItem(providerId, modelId, displayName, ctx, tags) {
  const isFreeModel = modelId === freeModelId;
  const locked = freeMode && !isFreeModel;
  const isSelected = modelId === currentModel && providerId === currentProvider;
  const item = document.createElement("div");
  item.className = "model-item" + (isSelected ? " selected" : "") + (locked ? " locked" : "");

  const check = document.createElement("span");
  check.className = "model-item-check";
  check.textContent = isSelected ? "✓" : "";

  const name = document.createElement("span");
  name.className = "model-item-name";
  name.textContent = modelDisplay(modelId, displayName);

  const badges = document.createElement("span");
  badges.className = "model-item-badges";

  if (isFreeModel) {
    const freeBadge = document.createElement("span");
    freeBadge.className = "tag-badge free";
    freeBadge.textContent = "free";
    badges.appendChild(freeBadge);
  }

  if (ctx) {
    const ctxBadge = document.createElement("span");
    ctxBadge.className = "ctx-badge";
    ctxBadge.textContent = ctx;
    badges.appendChild(ctxBadge);
  }

  for (const tag of (tags || []).slice(0, 2)) {
    const tagBadge = document.createElement("span");
    tagBadge.className = "tag-badge " + tag;
    tagBadge.textContent = tag;
    badges.appendChild(tagBadge);
  }

  if (locked) {
    const lock = document.createElement("span");
    lock.className = "model-item-lock";
    lock.textContent = "🔒";
    badges.appendChild(lock);
  }

  item.appendChild(check);
  item.appendChild(name);
  item.appendChild(badges);

  item.addEventListener("click", () => {
    if (locked) {
      vscode.postMessage({ type: "needApiKey" });
      closeModelDropdown();
      return;
    }
    currentProvider = providerId;
    currentModel = modelId;
    updateModelPill();
    closeModelDropdown();
  });

  return item;
}

function updateModelPill() {
  if (currentModel) {
    let displayName = currentModel;
    const curated = (curatedModels[currentProvider] || []).find(m => m.id === currentModel);
    if (curated) displayName = curated.name;
    else {
      const apiM = (allModels[currentProvider] || []).find(m => m.id === currentModel);
      if (apiM && apiM.name) displayName = apiM.name;
    }
    displayName = modelDisplay(currentModel, displayName);
    const shortName = displayName.length > 24 ? displayName.slice(0, 22) + "…" : displayName;
    modelPillLabel.textContent = shortName;
    modelPillIcon.textContent = getProviderIcon(currentProvider);
  } else {
    modelPillLabel.textContent = "Select model";
    modelPillIcon.textContent = "🤖";
  }
}

/* ── Send ── */
sendBtn.addEventListener("click", () => {
  if (streaming) {
    vscode.postMessage({ type: "stop" });
  } else {
    send();
  }
});

inputEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (streaming) vscode.postMessage({ type: "stop" });
    else send();
  }
});

inputEl.addEventListener("input", () => {
  inputEl.style.height = "auto";
  inputEl.style.height = Math.min(inputEl.scrollHeight, 150) + "px";
});

function send() {
  const text = inputEl.value.trim();
  if (!text) return;
  if (!currentProvider || !currentModel) {
    modelDropdown.classList.add("open");
    modelSearch.value = "";
    renderModelList("");
    return;
  }
  inputEl.value = "";
  inputEl.style.height = "auto";
  vscode.postMessage({ type: "orchestratedSend", provider: currentProvider, model: currentModel, text, mode: currentMode });
}

/* ── Messages ── */
function addMessage(role, content) {
  const div = document.createElement("div");
  div.className = "message " + role;
  const body = role === "assistant"
    ? '<div class="md">' + mdToHtml(content) + "</div>"
    : escapeHtml(content);
  div.innerHTML = '<span class="role-label">' + role + "</span>" + body;
  if (role === "assistant" && content) appendActionBtns(div, content);
  messagesEl.appendChild(div);
  scrollToBottom();
  return div;
}

function appendActionBtns(container, rawContent) {
  const insertBtn = document.createElement("button");
  insertBtn.className = "msg-action-btn btn-insert";
  insertBtn.textContent = "Insert";
  insertBtn.addEventListener("click", () => {
    vscode.postMessage({ type: "insertToEditor", content: rawContent });
    insertBtn.textContent = "Done";
    setTimeout(() => { insertBtn.textContent = "Insert"; }, 1500);
  });
  container.appendChild(insertBtn);

  const copyBtn = document.createElement("button");
  copyBtn.className = "msg-action-btn btn-copy";
  copyBtn.textContent = "Copy";
  copyBtn.addEventListener("click", () => {
    vscode.postMessage({ type: "copy", content: rawContent });
    copyBtn.textContent = "Copied";
    setTimeout(() => { copyBtn.textContent = "Copy"; }, 1500);
  });
  container.appendChild(copyBtn);
}

function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function mdToHtml(text) {
  if (typeof window.renderMarkdown === "function") {
    try { return window.renderMarkdown(text || ""); } catch (e) {}
  }
  return escapeHtml(text || "");
}

function scrollToBottom() {
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function appendThinkingBlock(cssClass, label, content) {
  const text = (content || "").trim();
  if (!text) return;
  if (!thoughtEl) {
    thoughtStart = Date.now();
    thoughtEl = document.createElement("details");
    thoughtEl.className = "thinking-block thinking";
    const summary = document.createElement("summary");
    summary.textContent = "Thinking\u2026";
    thoughtBodyEl = document.createElement("div");
    thoughtBodyEl.className = "thinking-content";
    thoughtEl.appendChild(summary);
    thoughtEl.appendChild(thoughtBodyEl);
    messagesEl.appendChild(thoughtEl);
  }
  const line = document.createElement("div");
  line.className = "thought-line";
  line.textContent = label === "Thinking" ? text : label + ": " + text;
  thoughtBodyEl.appendChild(line);
  updateThoughtSummary();
  scrollToBottom();
}

function updateThoughtSummary() {
  if (!thoughtEl) return;
  const secs = Math.max(1, Math.round((Date.now() - thoughtStart) / 1000));
  const summary = thoughtEl.querySelector("summary");
  if (summary) summary.textContent = "Thought for " + secs + "s";
}

function finalizeThought() {
  updateThoughtSummary();
  thoughtEl = null;
  thoughtBodyEl = null;
  thoughtStart = 0;
}

function stripThinkingTags(text) {
  if (!text) return "";
  let out = text
    .replace(/<plan>[\s\S]*?<\/plan>/gi, "")
    .replace(/<thinking>[\s\S]*?<\/thinking>/gi, "")
    .replace(/<reflection>[\s\S]*?<\/reflection>/gi, "");
  out = out.replace(/<\/?(plan|thinking|reflection)>/gi, "");
  const open = out.search(/<(plan|thinking|reflection)>[^]*$/i);
  if (open !== -1) out = out.slice(0, open);
  return out;
}

/* ── Settings Renderer ── */
function renderSettings(data) {
  let html = '<div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:12px;">'
    + '<span style="font-size:14px;font-weight:600;">Settings</span>'
    + '<button class="small-btn" data-act="closeSettings">Close</button></div>';

  html += '<div class="settings-section"><h3>Server</h3>';
  html += '<div class="setting-row"><label>Server URL</label>'
    + '<input type="url" value="' + escapeHtml(data.serverUrl) + '" id="settingServerUrl" />'
    + '<button class="small-btn" data-act="saveServerUrl">Save</button></div>';
  html += '</div>';

  html += '<div class="settings-section"><h3>Providers</h3>';
  for (const p of data.providers) {
    html += renderProviderCard(p);
  }
  html += '</div>';

  html += '<div class="settings-section"><h3>Preferences</h3>';
  html += '<div class="pref-row"><label>Auto-attach open file</label>'
    + '<input type="checkbox" ' + (data.fileContextEnabled ? 'checked' : '') + ' data-act="savePref" data-arg="fileContext.enabled" /></div>';
  html += '<div class="pref-row"><label>Inline completions</label>'
    + '<input type="checkbox" ' + (data.inlineCompletionsEnabled ? 'checked' : '') + ' data-act="savePref" data-arg="inlineCompletions.enabled" /></div>';
  html += '<div class="pref-row"><label>Inline provider</label>'
    + '<input type="text" value="' + escapeHtml(data.inlineProvider) + '" style="width:120px" data-act="savePref" data-arg="inlineCompletions.provider" /></div>';
  html += '<div class="pref-row"><label>Inline model</label>'
    + '<input type="text" value="' + escapeHtml(data.inlineModel) + '" style="width:120px" data-act="savePref" data-arg="inlineCompletions.model" /></div>';
  html += '</div>';

  if (data.serverConfig) {
    const sc = data.serverConfig;
    html += '<div class="settings-section"><h3>Agent</h3>';
    html += '<div class="setting-row"><label>Project root</label>'
      + '<input type="text" disabled value="' + escapeHtml(sc.agent.project_root) + '" style="opacity:0.6" /></div>';
    html += '<div class="setting-row"><label>Max iterations</label>'
      + '<input type="number" id="cfgMaxIter" value="' + sc.agent.max_iterations + '" style="width:80px" />'
      + '<button class="small-btn primary" data-act="saveServerCfg" data-arg="agent">Save</button></div>';
    html += '</div>';

    html += '<div class="settings-section"><h3>Memory</h3>';
    html += '<div class="pref-row"><label>Enabled</label><span>' + (sc.memory.enabled ? 'Yes' : 'No') + '</span></div>';
    html += '<div class="pref-row"><label>Top K</label><span>' + sc.memory.top_k + '</span></div>';
    html += '<div class="pref-row"><label>Max entries</label><span>' + sc.memory.max_entries + '</span></div>';
    html += '</div>';

    html += '<div class="settings-section"><h3>Context Management</h3>';
    html += '<div class="pref-row"><label>Enabled</label>'
      + '<input type="checkbox" id="cfgCtxEnabled" ' + (sc.context.enabled ? 'checked' : '') + ' /></div>';
    html += '<div class="setting-row"><label>Max tokens</label>'
      + '<input type="number" id="cfgCtxMaxTokens" value="' + (sc.context.max_tokens || '') + '" placeholder="Auto" style="width:100px" /></div>';
    html += '<div class="setting-row"><label>Reserve ratio</label>'
      + '<input type="number" id="cfgCtxReserve" value="' + sc.context.reserve_for_completion + '" step="0.05" min="0" max="1" style="width:80px" /></div>';
    html += '<div class="pref-row"><label>Strategy</label><span>' + escapeHtml(sc.context.strategy) + '</span></div>';
    html += '<button class="small-btn primary" data-act="saveServerCfg" data-arg="context" style="margin-top:4px">Save Context</button>';
    html += '</div>';

    if (sc.server) {
      html += '<div class="settings-section"><h3>Server</h3>';
      html += '<div class="pref-row"><label>Public URL</label><span>' + escapeHtml(sc.server.public_url || 'None') + '</span></div>';
      html += '<div class="pref-row"><label>Auth token</label><span>' + (sc.server.has_auth_token ? 'Configured' : 'None') + '</span></div>';
      html += '</div>';
    }
  }

  settingsPanel.innerHTML = html;
}

function renderProviderCard(p) {
  const checkedAttr = p.enabled ? "checked" : "";
  const enabledClass = p.enabled ? " enabled" : "";
  let body = '';

  if (p.needsApiKey) {
    const keyDisplay = p.hasKey ? "********" : "";
    body += '<div class="key-row">'
      + '<input type="password" placeholder="API Key" value="' + keyDisplay + '" id="key_' + p.id + '" />'
      + '<button class="small-btn" data-act="toggleKeyVis" data-arg="' + p.id + '">Show</button>'
      + '<button class="small-btn primary" data-act="saveKey" data-arg="' + p.id + '">Save</button>'
      + '<button class="small-btn" data-act="testProvider" data-arg="' + p.id + '">Test</button>'
      + '<span class="test-status" id="test_' + p.id + '"></span>'
      + '</div>';
    if (p.keyEnvHint) {
      body += '<div style="font-size:10px;color:var(--muted);margin-top:2px;">Or set env: ' + p.keyEnvHint + '</div>';
    }
  }

  if (p.needsUrl) {
    body += '<div class="setting-row" style="margin-top:4px;margin-bottom:0"><label style="min-width:60px">URL</label>'
      + '<input type="url" value="' + escapeHtml(p.url || '') + '" id="url_' + p.id + '" '
      + 'data-act="saveProviderField" data-arg="' + p.id + '" /></div>';
  }

  if (p.needsEndpointId) {
    body += '<div class="setting-row" style="margin-bottom:0"><label style="min-width:60px">Endpoint</label>'
      + '<input type="text" value="' + escapeHtml(p.endpointId || '') + '" id="eid_' + p.id + '" '
      + 'data-act="saveProviderField" data-arg="' + p.id + '" /></div>';
  }

  body += '<div class="setting-row" style="margin-top:4px;margin-bottom:0"><label style="min-width:60px">Model</label>'
    + '<input type="text" value="' + escapeHtml(p.defaultModel) + '" id="model_' + p.id + '" '
    + 'data-act="saveProviderField" data-arg="' + p.id + '" /></div>';

  return '<div class="provider-card' + enabledClass + '" id="card_' + p.id + '">'
    + '<div class="provider-card-header">'
    + '<input type="checkbox" ' + checkedAttr + ' data-act="toggleProvider" data-arg="' + p.id + '" />'
    + '<span class="provider-name">' + escapeHtml(p.label) + '</span>'
    + (p.hasKey ? '<span style="color:var(--success);font-size:11px;">Key saved</span>' : '')
    + '</div>'
    + '<div class="provider-card-body">' + body + '</div>'
    + '</div>';
}

/* ── Settings Actions ── */
function toggleProvider(id, enabled) {
  const card = document.getElementById("card_" + id);
  if (card) {
    if (enabled) card.classList.add("enabled");
    else card.classList.remove("enabled");
  }
  const modelInput = document.getElementById("model_" + id);
  const urlInput = document.getElementById("url_" + id);
  const eidInput = document.getElementById("eid_" + id);
  vscode.postMessage({
    type: "saveProviderConfig",
    providerId: id,
    enabled: enabled,
    defaultModel: modelInput ? modelInput.value : "",
    url: urlInput ? urlInput.value : undefined,
    endpointId: eidInput ? eidInput.value : undefined,
  });
}

function saveProviderField(id) {
  const card = document.getElementById("card_" + id);
  const enabled = card ? card.classList.contains("enabled") : false;
  const modelInput = document.getElementById("model_" + id);
  const urlInput = document.getElementById("url_" + id);
  const eidInput = document.getElementById("eid_" + id);
  vscode.postMessage({
    type: "saveProviderConfig",
    providerId: id,
    enabled: enabled,
    defaultModel: modelInput ? modelInput.value : "",
    url: urlInput ? urlInput.value : undefined,
    endpointId: eidInput ? eidInput.value : undefined,
  });
}

function saveKey(id) {
  const input = document.getElementById("key_" + id);
  if (!input) return;
  const val = input.value;
  if (val === "********") return;
  vscode.postMessage({ type: "saveApiKey", providerId: id, key: val });
  input.type = "password";
  input.value = "********";
}

function toggleKeyVis(id) {
  const input = document.getElementById("key_" + id);
  if (!input) return;
  input.type = input.type === "password" ? "text" : "password";
}

function testProvider(id) {
  const modelInput = document.getElementById("model_" + id);
  vscode.postMessage({ type: "testProvider", providerId: id, model: modelInput ? modelInput.value : "" });
}

function saveServerUrl() {
  const input = document.getElementById("settingServerUrl");
  if (input) vscode.postMessage({ type: "updateServerUrl", url: input.value });
}

function savePref(key, value) {
  vscode.postMessage({ type: "updatePreference", key, value });
}

function saveServerCfg(section) {
  const payload = {};
  if (section === "agent") {
    const maxIter = document.getElementById("cfgMaxIter");
    if (maxIter) payload.agent = { max_iterations: parseInt(maxIter.value, 10) || 25 };
  } else if (section === "context") {
    const enabled = document.getElementById("cfgCtxEnabled");
    const maxTok = document.getElementById("cfgCtxMaxTokens");
    const reserve = document.getElementById("cfgCtxReserve");
    payload.context = {
      enabled: enabled ? enabled.checked : true,
      max_tokens: maxTok && maxTok.value ? parseInt(maxTok.value, 10) : null,
      reserve_for_completion: reserve ? parseFloat(reserve.value) : 0.25,
    };
  }
  vscode.postMessage({ type: "updateServerConfig", payload });
}

/* ── Message Handler ── */
window.addEventListener("message", (event) => {
  const msg = event.data;
  switch (msg.type) {
    case "curatedCatalog":
      curatedModels = msg.curatedModels || {};
      providerMeta = msg.providerMeta || {};
      builtinProviders = msg.builtinProviders || [];
      if (modelDropdown.classList.contains("open")) {
        renderProviderTabs();
        renderModelList(modelSearch ? modelSearch.value.toLowerCase() : "");
      }
      break;

    case "providers":
      allProviders = msg.providers || [];
      for (const p of allProviders) {
        if (!allModels[p.id]) {
          vscode.postMessage({ type: "loadModels", provider: p.id });
        }
      }
      break;

    case "models":
      if (msg.provider) {
        allModels[msg.provider] = msg.models || [];
        if (modelDropdown.classList.contains("open")) {
          renderProviderTabs();
          renderModelList(modelSearch ? modelSearch.value.toLowerCase() : "");
        }
      }
      break;

    case "restoreSelections":
      if (msg.provider) currentProvider = msg.provider;
      if (msg.model) currentModel = msg.model;
      updateModelPill();
      if (msg.mode) {
        switchMode(msg.mode);
      }
      break;

    case "authMode":
      freeMode = !!msg.free;
      if (msg.freeModelId) freeModelId = msg.freeModelId;
      if (msg.freeModelLabel) freeModelLabel = msg.freeModelLabel;
      if (freeMode) {
        currentProvider = "getaibd";
        currentModel = freeModelId;
      }
      if (upgradeBtn) upgradeBtn.style.display = freeMode ? "inline-block" : "none";
      updateModelPill();
      if (modelDropdown.classList.contains("open")) {
        renderProviderTabs();
        renderModelList(modelSearch ? modelSearch.value.toLowerCase() : "");
      }
      break;

    case "sessions":
      sessionList = msg.sessions || [];
      activeSessionId = msg.activeId || "";
      renderSessions();
      break;

    case "clearMessages":
      messagesEl.innerHTML = "";
      streamEl = null;
      streamContent = "";
      agentTextEl = null;
      agentTextContent = "";
      break;

    case "setAgentMode":
      switchMode("agent");
      break;

    case "modeDetected": {
      const detected = msg.mode || "ask";
      const badge = document.createElement("div");
      badge.className = "context-compressed-msg";
      badge.textContent = "Mode: " + detected;
      messagesEl.appendChild(badge);
      messagesEl.scrollTop = messagesEl.scrollHeight;
      break;
    }

    case "showSettings":
      settingsOpen = true;
      settingsBtn.classList.add("active");
      messagesEl.style.display = "none";
      spinnerEl.style.display = "none";
      settingsPanel.classList.add("visible");
      vscode.postMessage({ type: "getSettings" });
      break;

    case "settings":
      renderSettings(msg);
      break;

    case "testResult": {
      const el = document.getElementById("test_" + msg.providerId);
      if (el) {
        el.className = "test-status " + msg.status;
        if (msg.status === "ok") el.textContent = "Connected";
        else if (msg.status === "testing") el.textContent = "Testing...";
        else el.textContent = msg.error ? msg.error.substring(0, 60) : "Failed";
      }
      break;
    }

    case "addMessage":
      addMessage(msg.role, msg.content);
      break;

    case "addContext": {
      const block = document.createElement("div");
      block.className = "context-block";
      block.innerHTML = '<span class="label">' + escapeHtml(msg.label) + '</span><pre>' + escapeHtml(msg.code) + '</pre>';
      messagesEl.appendChild(block);
      scrollToBottom();
      break;
    }

    case "streamStart":
      streaming = true;
      streamContent = "";
      sendBtn.innerHTML = "&#9632;";
      sendBtn.classList.add("stop");
      spinnerEl.classList.add("visible");
      reconnectBanner.classList.remove("visible");
      streamEl = addMessage("assistant", "");
      break;

    case "streamToken":
      if (streamEl) {
        streamContent += msg.content;
        streamEl.innerHTML = '<span class="role-label">assistant</span><div class="md">' + mdToHtml(streamContent) + "</div>";
        scrollToBottom();
      }
      break;

    case "streamEnd":
      streaming = false;
      if (streamEl && streamContent) appendActionBtns(streamEl, streamContent);
      streamEl = null;
      streamContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      reconnectBanner.classList.remove("visible");
      break;

    case "streamError": {
      streaming = false;
      streamEl = null;
      streamContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      reconnectBanner.classList.remove("visible");
      const errDiv = document.createElement("div");
      errDiv.className = "error-msg";
      errDiv.textContent = "Error: " + msg.error;
      messagesEl.appendChild(errDiv);
      scrollToBottom();
      break;
    }

    case "reconnecting":
      reconnectBanner.textContent = "Reconnecting... (attempt " + msg.attempt + "/" + msg.max + ")";
      reconnectBanner.classList.add("visible");
      spinnerEl.classList.remove("visible");
      break;

    case "streamRetry":
      streamContent = "";
      if (streamEl) streamEl.innerHTML = '<span class="role-label">assistant</span>';
      reconnectBanner.classList.remove("visible");
      spinnerEl.classList.add("visible");
      break;

    case "agentStart":
      streaming = true;
      agentTextEl = null;
      agentTextContent = "";
      thoughtEl = null;
      thoughtBodyEl = null;
      thoughtStart = 0;
      sendBtn.innerHTML = "&#9632;";
      sendBtn.classList.add("stop");
      spinnerEl.textContent = "Agent working...";
      spinnerEl.classList.add("visible");
      break;

    case "agentToolCall": {
      agentTextEl = null;
      const tcDiv = document.createElement("div");
      tcDiv.className = "tool-call";
      tcDiv.innerHTML = '<span class="tool-name">' + escapeHtml(msg.name) + '</span>'
        + '<div class="tool-args">' + escapeHtml(JSON.stringify(msg.arguments, null, 2)) + '</div>';
      messagesEl.appendChild(tcDiv);
      scrollToBottom();
      break;
    }

    case "agentToolResult": {
      agentTextEl = null;
      let t = msg.result == null
        ? ""
        : (typeof msg.result === "string" ? msg.result : JSON.stringify(msg.result, null, 2));
      if (!t || t === "undefined" || t === "null") break;
      const trDiv = document.createElement("div");
      trDiv.className = "tool-result";
      trDiv.textContent = t.length > 500 ? t.slice(0, 500) + "..." : t;
      messagesEl.appendChild(trDiv);
      scrollToBottom();
      break;
    }

    case "agentText": {
      if (!agentTextEl) {
        agentTextContent = "";
      }
      agentTextContent += msg.content || "";
      const clean = stripThinkingTags(agentTextContent);
      if (!clean.trim()) break;
      if (!agentTextEl) {
        agentTextEl = addMessage("assistant", "");
      }
      agentTextEl.innerHTML = '<span class="role-label">assistant</span><div class="md">' + mdToHtml(clean) + "</div>";
      scrollToBottom();
      break;
    }

    case "agentPlanning":
      agentTextEl = null;
      appendThinkingBlock("plan", "Plan", msg.content);
      break;

    case "agentThinking":
      agentTextEl = null;
      appendThinkingBlock("thinking", "Thinking", msg.content);
      break;

    case "agentReflecting":
      agentTextEl = null;
      appendThinkingBlock("reflection", "Reflection", msg.content);
      break;

    case "agentReplanning":
      agentTextEl = null;
      appendThinkingBlock("replan", "Re-planning", msg.content);
      break;

    case "agentContextCompressed": {
      agentTextEl = null;
      const ccDiv = document.createElement("div");
      ccDiv.className = "context-compressed-msg";
      ccDiv.textContent = msg.content || "Context compressed";
      messagesEl.appendChild(ccDiv);
      scrollToBottom();
      break;
    }

    case "agentDone":
      finalizeThought();
      if (agentTextEl) {
        const clean = stripThinkingTags(agentTextContent);
        if (clean.trim()) appendActionBtns(agentTextEl, clean);
      } else if (msg.content) {
        const clean = stripThinkingTags(msg.content);
        if (clean.trim()) {
          const el = addMessage("assistant", clean);
          appendActionBtns(el, clean);
        }
      }
      agentTextEl = null;
      agentTextContent = "";
      break;

    case "agentComplete":
      streaming = false;
      finalizeThought();
      if (agentTextEl) {
        const clean = stripThinkingTags(agentTextContent);
        if (clean.trim()) appendActionBtns(agentTextEl, clean);
      }
      agentTextEl = null;
      agentTextContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      {
        const cd = document.createElement("div");
        cd.className = "agent-status";
        cd.textContent = "Agent completed (" + msg.iterations + " iterations)";
        messagesEl.appendChild(cd);
        scrollToBottom();
      }
      break;

    case "agentError":
      streaming = false;
      finalizeThought();
      agentTextEl = null;
      agentTextContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      {
        const ae = document.createElement("div");
        ae.className = "error-msg";
        ae.textContent = "Agent error: " + msg.error;
        messagesEl.appendChild(ae);
        scrollToBottom();
      }
      break;

    case "approvalStatus": {
      const apDiv = document.createElement("div");
      apDiv.className = "agent-status";
      apDiv.textContent = msg.approved ? "Approved: " + msg.toolName : "Denied: " + msg.toolName;
      messagesEl.appendChild(apDiv);
      scrollToBottom();
      break;
    }

    case "error":
      break;
  }
});

vscode.postMessage({ type: "ready" });
try { var __b = document.getElementById("brand"); if (__b) { __b.textContent = "GetAIBD"; } } catch (_) {}
</script>
</body>
</html>`;
}
