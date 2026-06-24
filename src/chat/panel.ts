import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";
import * as crypto from "crypto";
import {
  fetchProviders,
  fetchModels,
  fetchAccountStatus,
  streamChat,
  streamAgent,
  streamOrchestrated,
  sendApproval,
  sendTerminalResult,
  sendAskResult,
  testProviderConnection,
  type ChatMessage,
  type FileEdit,
} from "../client";
import { scanText, formatWarning } from "../safetype/detector";
import { ensureEngine, restartEngine } from "../engine/manager";
import { PatchPreviewPanel } from "./patchPreview";
import { EditReviewManager } from "./editReview";
import { AgentTerminal } from "./terminal";
import { createFreeSession } from "../free";
import {
  getServerUrl,
  authHeaders,
  getApiKey,
  setApiKey,
  isFreeToken,
  FREE_MODEL_ID,
  FREE_MODEL_LABEL,
  CREDIT_FLOOR,
  BILLING_URL,
} from "../util/config";
import { ProviderStore, BUILTIN_PROVIDERS, CURATED_MODELS, PROVIDER_META } from "../settings/providerStore";
import {
  contextFingerprint,
  estimatePayload,
  formatContextEstimate,
  truncateFileContent,
  HISTORY_CHAR_BUDGET,
} from "../util/contextBudget";

interface WebviewMessage {
  type: string;
  [key: string]: unknown;
}

interface CheckpointBaseline {
  path: string;
  before: string;
  existed: boolean;
}

interface HistoryEntry {
  kind: "message" | "context" | "fileEdit" | "checkpoint";
  role?: string;
  content: string;
  label?: string;
  path?: string;
  editOld?: string;
  editNew?: string;
  tooLarge?: boolean;
  editStatus?: "pending" | "accept" | "reject";
  turnId?: string;
  baselines?: CheckpointBaseline[];
}

interface SessionMeta {
  id: string;
  title: string;
  createdAt: number;
}

const SESSIONS_KEY = "getaibd.sessions";
const ACTIVE_SESSION_KEY = "getaibd.activeSession";
const SESSION_HISTORY_PREFIX = "getaibd.history.";
const PROVIDER_KEY = "getaibd.lastProvider";
const MODEL_KEY = "getaibd.lastModel";
const MODE_KEY = "getaibd.lastMode";
const COMPRESS_KEY = "getaibd.compress";
const REASONING_KEY = "getaibd.reasoningEffort";
const ALWAYS_ALLOW_KEY = "getaibd.alwaysAllowTools";
const MAX_RECONNECT = 3;

/**
 * A `Memento`-compatible store backed by a JSON file with **synchronous** writes.
 *
 * VS Code's `workspaceState`/`globalState` writes are async and only flushed
 * lazily; when the extension host is torn down to install an update, a just-
 * written value can be lost — which is why the most recent chat would vanish
 * after an update. Writing through to disk synchronously makes each save durable
 * the moment it happens, so nothing is lost across updates or reloads.
 */
class DiskStore implements vscode.Memento {
  private data: Record<string, unknown> = {};

  constructor(private readonly file: string) {
    try {
      this.data = JSON.parse(fs.readFileSync(file, "utf8")) as Record<string, unknown>;
    } catch {
      this.data = {};
    }
  }

  keys(): readonly string[] {
    return Object.keys(this.data);
  }

  get<T>(key: string): T | undefined;
  get<T>(key: string, defaultValue: T): T;
  get<T>(key: string, defaultValue?: T): T | undefined {
    return (Object.prototype.hasOwnProperty.call(this.data, key)
      ? this.data[key]
      : defaultValue) as T | undefined;
  }

  update(key: string, value: unknown): Thenable<void> {
    if (value === undefined) {
      delete this.data[key];
    } else {
      this.data[key] = value;
    }
    try {
      fs.mkdirSync(path.dirname(this.file), { recursive: true });
      // Atomic-ish write: temp file + rename so a crash mid-write can't corrupt
      // the store (rename is atomic on the same filesystem).
      const tmp = `${this.file}.tmp`;
      fs.writeFileSync(tmp, JSON.stringify(this.data));
      fs.renameSync(tmp, this.file);
    } catch {
      /* best effort — persistence must never crash the chat */
    }
    return Promise.resolve();
  }
}

/**
 * Builds the per-workspace session store path under the extension's global
 * storage (which survives extension updates), keyed by the open workspace so
 * each project keeps its own conversations.
 */
function sessionStoreFile(context: vscode.ExtensionContext): string {
  const wsId = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "__noworkspace__";
  const hash = crypto.createHash("sha1").update(wsId).digest("hex").slice(0, 16);
  return path.join(context.globalStorageUri.fsPath, "chat-sessions", `${hash}.json`);
}

/**
 * One-time import of chat sessions/history from the legacy `workspaceState`
 * store into the durable disk store, so existing conversations carry over.
 */
function migrateSessionsFromMemento(disk: DiskStore, legacy: vscode.Memento): void {
  if (disk.keys().length > 0) {
    return; // already migrated / has data
  }
  let copied = false;
  for (const key of legacy.keys()) {
    if (
      key === SESSIONS_KEY ||
      key === ACTIVE_SESSION_KEY ||
      key.startsWith(SESSION_HISTORY_PREFIX)
    ) {
      const value = legacy.get(key);
      if (value !== undefined) {
        void disk.update(key, value);
        copied = true;
      }
    }
  }
  if (!copied) {
    // Mark as initialised so we don't re-scan legacy state on every launch.
    void disk.update("getaibd.migrated", true);
  }
}

export class ChatPanel implements vscode.WebviewViewProvider {
  static readonly viewType = "getaibd.chatView";
  private static instance: ChatPanel | undefined;
  private view: vscode.WebviewView | undefined;
  private readonly globalState: vscode.Memento;
  // Per-workspace store for chat sessions + history so each project/window keeps
  // its own conversations (globalState is shared across every VS Code window).
  private readonly sessionStore: vscode.Memento;
  private readonly context: vscode.ExtensionContext;
  private readonly store: ProviderStore;
  private abortController: AbortController | undefined;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  private disposables: vscode.Disposable[] = [];
  private sessions: SessionMeta[] = [];
  private activeSessionId = "";
  private history: HistoryEntry[] = [];
  private currentStreamContent = "";
  private fileEdits = new Map<string, { path: string; originalOld: string; latestNew: string }>();
  private editContents = new Map<string, { originalOld: string; latestNew: string }>();
  private editReview = new EditReviewManager(vscode.workspace.workspaceFolders?.[0]?.uri);
  private pendingApprovals = new Map<string, string | undefined>();
  private pendingApprovalTools = new Map<string, string>();
  private pendingAsks = new Map<string, string | undefined>();
  private lastDiffPath: string | undefined;
  private currentTurnId: string | undefined;
  private stepLimitHit = false;
  private agentTerminal = new AgentTerminal(vscode.workspace.workspaceFolders?.[0]?.uri);
  private activeTerminalReq: { requestId: string; sessionId: string | undefined } | undefined;
  /** Skip re-sending unchanged open-file context on consecutive agent turns. */
  private lastOpenFileFingerprint: string | null = null;
  /** Manual context blocks already included in a prior turn this session. */
  private injectedContextFingerprints = new Set<string>();

  constructor(context: vscode.ExtensionContext) {
    this.globalState = context.globalState;
    // Durable, synchronous on-disk store so the latest chat survives extension
    // updates/reloads (workspaceState writes can be lost when the host is torn
    // down before they flush). Existing conversations are migrated on first run.
    const disk = new DiskStore(sessionStoreFile(context));
    migrateSessionsFromMemento(disk, context.workspaceState);
    this.sessionStore = disk;
    this.context = context;
    this.store = new ProviderStore(context.globalState, context.secrets);
    this.loadSessions();
    ChatPanel.instance = this;
  }

  /** Registers the chat as a full-height sidebar webview view. */
  static register(context: vscode.ExtensionContext): vscode.Disposable {
    const provider = new ChatPanel(context);
    const diffProvider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent(uri) {
        const edit = provider.editContents.get(decodeURIComponent(uri.query));
        if (!edit) {return "";}
        return uri.scheme === "getaibd-diff-new" ? edit.latestNew : edit.originalOld;
      },
    };
    provider.editReview.setOnResolved((relPath, action) => {
      provider.post({ type: "editResolved", path: relPath, action });
      provider.fileEdits.delete(relPath);
      provider.markEditResolved(relPath, action);
    });
    provider.editReview.setOnEditChanged((relPath, originalOld, latestNew) => {
      provider.syncEditAfterBlockOp(relPath, originalOld, latestNew);
    });
    context.subscriptions.push(
      vscode.workspace.registerTextDocumentContentProvider("getaibd-diff", diffProvider),
      vscode.workspace.registerTextDocumentContentProvider("getaibd-diff-new", diffProvider),
      ...provider.editReview.register(),
      vscode.commands.registerCommand("getaibd.acceptActiveDiff", () => {
        if (provider.lastDiffPath) {provider.keepEdit(provider.lastDiffPath);}
        void vscode.commands.executeCommand("workbench.action.closeActiveEditor");
      }),
      vscode.commands.registerCommand("getaibd.rejectActiveDiff", () => {
        if (provider.lastDiffPath) {void provider.undoEdit(provider.lastDiffPath);}
        void vscode.commands.executeCommand("workbench.action.closeActiveEditor");
      }),
      vscode.window.tabGroups.onDidChangeTabs(() => provider.updateDiffContext()),
    );
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

  /**
   * Restarts the engine so it re-reads the new GetAIBD key, then refreshes the
   * webview (auth mode, models, balance). Falls back to a Reload Window prompt.
   */
  private async applyGetaibdKeyChange() {
    try {
      await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Notification, title: "GetAIBD: applying API key…" },
        async () => {
          await restartEngine(this.context);
        },
      );
      await this.refreshAuthMode();
      await this.refreshModels();
      void vscode.commands.executeCommand("getaibd.refreshBalance");
      void vscode.window.showInformationMessage("GetAIBD API key applied.");
    } catch {
      const choice = await vscode.window.showWarningMessage(
        "GetAIBD: couldn't restart the engine automatically. Reload the window to apply the key.",
        "Reload Window",
      );
      if (choice === "Reload Window") {
        void vscode.commands.executeCommand("workbench.action.reloadWindow");
      }
    }
  }

  /** Re-reads the stored credential and tells the webview whether it is free-tier. */
  async refreshAuthMode() {
    const key = await getApiKey(this.context.secrets);
    const free = isFreeToken(key);
    let hasPlan = !free;
    if (key && !free) {
      const status = await fetchAccountStatus(key);
      hasPlan = !!status?.hasPlan;
    }
    this.post({
      type: "authMode",
      free,
      hasPlan,
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
        this.refreshCreditsBalance(provider);
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
        this.refreshCreditsBalance(provider);
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
        this.cancelActiveTerminal();
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
          await this.sendOrchestrated(
            msg.provider as string,
            msg.model as string,
            msg.text as string,
            msg.mode as string,
            (msg.reasoningEffort as string | undefined) ?? undefined,
            !!msg.compress,
          );
        }
        break;
      case "compressChanged":
        await this.globalState.update(COMPRESS_KEY, !!msg.compress);
        break;
      case "reasoningChanged": {
        const effort = String(msg.reasoningEffort || "medium");
        await this.globalState.update(REASONING_KEY, effort);
        break;
      }
      case "openDiff":
        if (msg.path) {await this.openDiff(msg.path as string);}
        break;
      case "restoreCheckpoint":
        if (msg.turnId) {await this.restoreCheckpoint(msg.turnId as string);}
        break;
      case "keepEdits":
        this.keepEdits();
        break;
      case "undoEdits":
        await this.undoEdits();
        break;
      case "keepEdit":
        if (msg.path) {this.keepEdit(msg.path as string);}
        break;
      case "undoEdit":
        if (msg.path) {await this.undoEdit(msg.path as string);}
        break;
      case "approvalResponse":
        if (msg.requestId) {await this.resolveApproval(msg.requestId as string, !!msg.approved, !!msg.always);}
        break;
      case "askResponse":
        if (msg.requestId) {await this.resolveAsk(msg.requestId as string, (msg.answer as string) ?? "");}
        break;
      case "removeAlwaysAllow":
        if (msg.tool) {
          const list = this.getAlwaysAllow().filter((t) => t !== msg.tool);
          await this.globalState.update(ALWAYS_ALLOW_KEY, list);
          await this.sendSettings();
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
        await this.promptPaymentRequired();
        break;
      case "needPlan":
        await this.promptPlanRequired();
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
        if (msg.providerId === "getaibd") {
          await setApiKey(this.context.secrets, (msg.key as string) || "");
          await this.applyGetaibdKeyChange();
        } else {
          await this.store.setApiKey(msg.providerId as string, msg.key as string);
        }
        await this.sendSettings();
        break;
      case "removeApiKey":
        if (msg.providerId === "getaibd") {
          try {
            const free = await createFreeSession(this.context);
            await setApiKey(this.context.secrets, free.token);
          } catch {
            await setApiKey(this.context.secrets, "");
          }
          await this.applyGetaibdKeyChange();
        } else {
          await this.store.setApiKey(msg.providerId as string, "");
        }
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
    this.post({ type: "bootStatus", state: "loading", text: "Getting ready…" });
    try {
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
    } finally {
      this.ready = true;
      this.post({ type: "bootStatus", state: "ready" });
      const queued = this.pending.splice(0);
      for (const fn of queued) {
        fn();
      }
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

  /** Refresh the status-bar credit counter after a billable GetAIBD call. */
  private refreshCreditsBalance(provider?: string): void {
    if (provider && provider !== "getaibd") {
      return;
    }
    void vscode.commands.executeCommand("getaibd.refreshBalance");
  }

  /** Detects payment-required errors; top up for paid keys, add key for free tier. */
  private maybeHandlePaymentError(error: string): boolean {
    if (/\b403\b|subscribe to a package|developer api/i.test(error)) {
      void this.promptPlanRequired(error);
      return true;
    }
    if (!/\b402\b|Free limit|requires your own|exceed|top up|Credits running low|running low/i.test(error)) {
      return false;
    }
    void this.promptPaymentRequired(error);
    return true;
  }

  /** Blocks paid GetAIBD usage when credits are at or below the platform floor. */
  private async ensureCreditsAllowance(): Promise<boolean> {
    const key = await getApiKey(this.context.secrets);
    if (!key || isFreeToken(key)) {
      return true;
    }
    const status = await fetchAccountStatus(key);
    if (!status || status.free) {
      return true;
    }
    if (status.hasPlan === false) {
      void this.promptPlanRequired();
      return false;
    }
    const floor = status.creditFloor ?? CREDIT_FLOOR;
    const balance = status.creditsBalance ?? 0;
    if (balance <= floor) {
      void this.promptLowCredits(balance, floor);
      return false;
    }
    return true;
  }

  private async promptLowCredits(balance: number, floor: number): Promise<void> {
    const choice = await vscode.window.showWarningMessage(
      `GetAIBD credits are too low (${balance.toLocaleString()} remaining). ` +
        `Top up to keep using the agent (minimum ${floor} credits).`,
      "Top Up Credits",
      "Refresh Balance",
    );
    if (choice === "Top Up Credits") {
      void vscode.env.openExternal(vscode.Uri.parse(BILLING_URL));
    } else if (choice === "Refresh Balance") {
      void vscode.commands.executeCommand("getaibd.refreshBalance");
    }
  }

  /** Paid key without a package — subscribe before using models in the extension. */
  private async promptPlanRequired(detail?: string) {
    const summary =
      detail && detail.length < 220
        ? detail.replace(/^Agent error: Provider error: getaibd:\s*/i, "")
        : "Subscribe to a GetAIBD package to use models in the extension.";
    const choice = await vscode.window.showWarningMessage(summary, "View Packages", "Refresh");
    if (choice === "View Packages") {
      void vscode.env.openExternal(vscode.Uri.parse(BILLING_URL));
    } else if (choice === "Refresh") {
      void vscode.commands.executeCommand("getaibd.refreshBalance");
      await this.refreshAuthMode();
    }
  }

  /** Free tier → add a key; paid key with low credits → top up. */
  private async promptPaymentRequired(detail?: string) {
    const key = await getApiKey(this.context.secrets);
    const isPaid = !!key && !isFreeToken(key);
    const text = (detail ?? "").toLowerCase();
    const isCreditIssue =
      isPaid &&
      (/exceed|balance|running low|top up|credit/i.test(text) ||
        !/free tier|free limit|add a getaibd api key|needs your own api key/i.test(text));

    if (isCreditIssue) {
      const summary =
        detail && detail.length < 220
          ? detail.replace(/^Agent error: Provider error: getaibd:\s*/i, "")
          : "Your GetAIBD credits are too low for this request. Top up to continue.";
      const choice = await vscode.window.showWarningMessage(summary, "Top Up Credits", "Refresh Balance");
      if (choice === "Top Up Credits") {
        void vscode.env.openExternal(vscode.Uri.parse(BILLING_URL));
      } else if (choice === "Refresh Balance") {
        void vscode.commands.executeCommand("getaibd.refreshBalance");
      }
      return;
    }

    const base =
      detail && /Free limit/i.test(detail)
        ? "You've used all your free days this month."
        : 'That model needs your own GetAIBD API key. The free tier only includes the "Auto" model.';
    const choice = await vscode.window.showInformationMessage(base, "Set API Key", "Get a Key");
    if (choice === "Set API Key") {
      await vscode.commands.executeCommand("getaibd.setApiKey");
    } else if (choice === "Get a Key") {
      void vscode.env.openExternal(vscode.Uri.parse("https://getaibd.com"));
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
    const compress =
      this.globalState.get<boolean>(COMPRESS_KEY) ??
      config.get<boolean>("chat.compressDefault", true);
    const reasoning =
      this.globalState.get<string>(REASONING_KEY) ??
      config.get<string>("chat.reasoningDefault", "medium");

    this.post({
      type: "restoreSelections",
      provider: lastProvider,
      model: lastModel,
      mode: lastMode,
      compress,
      reasoningEffort: reasoning,
    });
  }

  private async sendSettings() {
    const snapshot = await this.store.getFullSettingsSnapshot();
    const platformKey = await getApiKey(this.context.secrets);
    const hasRealKey = !!platformKey && !isFreeToken(platformKey);
    const providers = snapshot.providers.map((p) =>
      p.id === "getaibd" ? { ...p, hasKey: hasRealKey } : p,
    );
    this.post({
      type: "settings",
      ...snapshot,
      providers,
      alwaysAllow: this.getAlwaysAllow(),
    });
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

  private resetContextInjectionState() {
    this.lastOpenFileFingerprint = null;
    this.injectedContextFingerprints.clear();
  }

  private postContextEstimate(messages: ChatMessage[], input: string) {
    const est = estimatePayload(messages, input);
    this.post({
      type: "contextEstimate",
      chars: est.chars,
      tokensApprox: est.tokensApprox,
      fileAttachments: est.fileAttachments,
      historyTurns: est.historyTurns,
      label: formatContextEstimate(est),
    });
  }

  /** Attached context blocks from the UI (sent once per unique attachment per session). */
  private manualContextMessages(): ChatMessage[] {
    const out: ChatMessage[] = [];
    for (const e of this.history) {
      if (e.kind !== "context" || !e.content) {continue;}
      const label = e.label || "attachment";
      const fp = contextFingerprint(`ctx:${label}`, e.content);
      if (this.injectedContextFingerprints.has(fp)) {continue;}
      this.injectedContextFingerprints.add(fp);
      const snippet = truncateFileContent(e.content);
      out.push({
        role: "user",
        content: `[Context: ${label}]\n\`\`\`\n${snippet}\n\`\`\``,
      });
    }
    return out;
  }

  private async resolveAtMentions(text: string, skipPaths?: Set<string>): Promise<ChatMessage[]> {
    const mentionRe = /@([^\s]+)/g;
    const messages: ChatMessage[] = [];
    let m: RegExpExecArray | null;
    const resolved = new Set<string>();
    while ((m = mentionRe.exec(text)) !== null) {
      const ref = m[1];
      if (resolved.has(ref)) {continue;}
      if (skipPaths?.has(ref) || skipPaths?.has(`@${ref}`)) {continue;}
      resolved.add(ref);
      const files = await vscode.workspace.findFiles(ref, null, 1);
      if (files.length > 0) {
        const doc = await vscode.workspace.openTextDocument(files[0]);
        const full = doc.getText();
        const snippet = truncateFileContent(full);
        messages.push({ role: "user", content: `[File: ${ref}]\n\`\`\`\n${snippet}\n\`\`\`` });
        this.post({
          type: "addContext",
          label: `@${ref}`,
          code: full.slice(0, 500) + (full.length > 500 ? "\n..." : ""),
        });
      }
    }
    return messages;
  }

  private buildFileContext(skipPaths?: Set<string>): ChatMessage[] {
    const messages: ChatMessage[] = [];
    const config = vscode.workspace.getConfiguration("getaibd");
    if (!config.get<boolean>("fileContext.enabled", true)) {return messages;}
    const editor = vscode.window.activeTextEditor;
    if (!editor) {return messages;}
    const name = vscode.workspace.asRelativePath(editor.document.uri);
    if (skipPaths?.has(name)) {return messages;}
    const content = editor.document.getText();
    const fp = contextFingerprint(`open:${name}`, content);
    if (fp === this.lastOpenFileFingerprint) {return messages;}
    this.lastOpenFileFingerprint = fp;
    const snippet = truncateFileContent(content);
    messages.push({
      role: "user",
      content: `[Currently open file: ${name}]\n\`\`\`\n${snippet}\n\`\`\``,
    });
    return messages;
  }

  /** Agent path: @mentions only when the user typed them — no open file or attachment replay. */
  private async buildAgentContext(userText: string): Promise<ChatMessage[]> {
    if (!/@\S+/.test(userText)) {
      return [];
    }
    return this.resolveAtMentions(userText);
  }

  /** Simple chat: optional open file + attachments + @mentions. */
  private async buildExplicitContext(userText: string): Promise<ChatMessage[]> {
    const skip = new Set<string>();
    const manual = this.manualContextMessages();
    for (const m of manual) {
      const match = /^\[Context: ([^\]]+)\]/.exec(m.content);
      if (match) {skip.add(match[1]);}
    }
    const fileCtx = this.buildFileContext(skip);
    for (const m of fileCtx) {
      const match = /^\[Currently open file: ([^\]]+)\]/.exec(m.content);
      if (match) {skip.add(match[1]);}
    }
    const mentionCtx = await this.resolveAtMentions(userText, skip);
    return [...manual, ...fileCtx, ...mentionCtx];
  }

  /** Prior turns for the agent; drops the current user line (sent separately as `input`). */
  private buildPriorHistory(userText: string, explicitCtx: ChatMessage[]): ChatMessage[] {
    const turns = this.conversationMessages();
    const trimmed =
      turns.length > 0 &&
      turns[turns.length - 1].role === "user" &&
      turns[turns.length - 1].content === userText
        ? turns.slice(0, -1)
        : turns;
    return [...explicitCtx, ...trimmed];
  }

  private async sendUserMessage(provider: string, model: string, text: string) {
    if (!(await this.checkSecrets(text))) {return;}
    if (provider === "getaibd" && !(await this.ensureCreditsAllowance())) {return;}
    this.history.push({ kind: "message", role: "user", content: text });
    this.saveHistory();
    this.post({ type: "addMessage", role: "user", content: text });

    const apiKey = await this.store.getApiKey(provider);
    const explicitCtx = await this.buildExplicitContext(text);
    const messages: ChatMessage[] = [...explicitCtx, { role: "user", content: text }];
    this.postContextEstimate(explicitCtx, text);
    this.streamToPanel(provider, model, messages, apiKey);
  }

  private async sendAgentTask(provider: string, model: string, task: string) {
    if (!(await this.checkSecrets(task))) {return;}
    if (provider === "getaibd" && !(await this.ensureCreditsAllowance())) {return;}
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
      onApprovalRequired: (requestId, sessionId, toolName, args) => {
        this.handleApprovalRequest(requestId, sessionId, toolName, args);
      },
      onTerminalExec: (requestId, sessionId, args) => {
        void this.handleTerminalExec(requestId, sessionId, args);
      },
      onAskRequired: (requestId, sessionId, question, options, multiple) => {
        this.handleAskRequest(requestId, sessionId, question, options, multiple);
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
      onDiscardDraft: () => {
        this.post({ type: "agentDiscardDraft" });
      },
      onFileEdit: (edit) => {
        this.handleFileEdit(edit);
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
        this.refreshCreditsBalance(provider);
      },
      onError: (error) => {
        this.post({ type: "agentError", error });
        this.abortController = undefined;
        this.refreshCreditsBalance(provider);
        this.maybeHandlePaymentError(error);
      },
    }, { requireApproval: true, apiKey });
  }

  private async sendOrchestrated(
    provider: string,
    model: string,
    text: string,
    mode: string,
    reasoningEffort?: string,
    compress = false,
  ) {
    if (!(await this.checkSecrets(text))) {return;}
    if (provider === "getaibd" && !(await this.ensureCreditsAllowance())) {return;}
    this.stepLimitHit = false;
    const turnId = newId();
    this.currentTurnId = turnId;
    this.history.push({ kind: "checkpoint", content: text, turnId, baselines: [] });
    this.history.push({ kind: "message", role: "user", content: text, turnId });
    this.saveHistory();
    this.post({ type: "addMessage", role: "user", content: text, turnId, canRestore: true });
    const explicitCtx = await this.buildAgentContext(text);
    const priorHistory = this.buildPriorHistory(text, explicitCtx);
    this.postContextEstimate(priorHistory, text);
    this.post({ type: "agentStart" });

    const apiKey = await this.store.getApiKey(provider);
    const agentUseMemory = vscode.workspace
      .getConfiguration("getaibd")
      .get<boolean>("agent.useMemory", false);
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
      onApprovalRequired: (requestId, sessionId, toolName, args) => {
        this.handleApprovalRequest(requestId, sessionId, toolName, args);
      },
      onTerminalExec: (requestId, sessionId, args) => {
        void this.handleTerminalExec(requestId, sessionId, args);
      },
      onAskRequired: (requestId, sessionId, question, options, multiple) => {
        this.handleAskRequest(requestId, sessionId, question, options, multiple);
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
      onStepLimit: () => {
        this.stepLimitHit = true;
      },
      onDiscardDraft: () => {
        this.post({ type: "agentDiscardDraft" });
      },
      onFileEdit: (edit) => {
        this.handleFileEdit(edit);
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
        this.post({ type: "agentComplete", iterations, stepLimit: this.stepLimitHit });
        this.stepLimitHit = false;
        this.abortController = undefined;
        this.refreshCreditsBalance(provider);
      },
      onError: (error) => {
        this.post({ type: "agentError", error });
        this.abortController = undefined;
        this.refreshCreditsBalance(provider);
        this.maybeHandlePaymentError(error);
      },
    }, { apiKey, history: priorHistory, requireApproval: true, clientTerminal: AgentTerminal.supported, reasoningEffort, compress: provider === "getaibd" && compress, useMemory: agentUseMemory });
  }

  private handleFileEdit(edit: FileEdit) {
    this.recordTurnBaseline(edit);
    const existing = this.editContents.get(edit.path);
    const originalOld = existing ? existing.originalOld : (edit.old_content ?? "");
    const latestNew = edit.new_content ?? "";
    const tooLarge = !!edit.too_large;
    this.fileEdits.set(edit.path, { path: edit.path, originalOld, latestNew });
    this.editContents.set(edit.path, { originalOld, latestNew });
    this.editReview.addEdit(edit.path, originalOld, latestNew);
    const { additions, deletions } = diffStat(originalOld, latestNew);
    const diff = tooLarge ? { lines: [], truncated: true } : computeDiffHunks(originalOld, latestNew);
    this.post({
      type: "fileEdit",
      path: edit.path,
      additions,
      deletions,
      tooLarge,
      isNew: originalOld.length === 0 && latestNew.length > 0,
      diff,
    });
    this.recordEditHistory(edit.path, originalOld, latestNew, tooLarge);
    this.autoOpenEdit(edit.path);
  }

  /** Persists an agent edit in the session history so it survives reloads. */
  private recordEditHistory(path: string, editOld: string, editNew: string, tooLarge: boolean) {
    const existing = this.history.find((e) => e.kind === "fileEdit" && e.path === path);
    if (existing) {
      existing.editOld = editOld;
      existing.editNew = editNew;
      existing.tooLarge = tooLarge;
      existing.editStatus = "pending";
    } else {
      this.history.push({
        kind: "fileEdit",
        content: path,
        path,
        editOld,
        editNew,
        tooLarge,
        editStatus: "pending",
      });
    }
    this.saveHistory();
  }

  /** Captures the pre-edit content of a file the first time it changes in the current turn. */
  private recordTurnBaseline(edit: FileEdit) {
    const turnId = this.currentTurnId;
    if (!turnId) {return;}
    const cp = [...this.history].reverse().find((e) => e.kind === "checkpoint" && e.turnId === turnId);
    if (!cp) {return;}
    cp.baselines = cp.baselines ?? [];
    if (cp.baselines.some((b) => b.path === edit.path)) {return;}
    const before = edit.old_content ?? "";
    const existed = before.length > 0 || this.editContents.has(edit.path);
    cp.baselines.push({ path: edit.path, before, existed });
    this.saveHistory();
  }

  /** Reverts every file change from this checkpoint onward and rolls the chat back to it. */
  private async restoreCheckpoint(turnId: string) {
    const idx = this.history.findIndex((e) => e.kind === "checkpoint" && e.turnId === turnId);
    if (idx < 0) {return;}
    const confirm = await vscode.window.showWarningMessage(
      "Restore to this checkpoint? This reverts file changes made from this point onward and removes later messages.",
      { modal: true },
      "Restore",
    );
    if (confirm !== "Restore") {return;}
    this.abortController?.abort();
    this.abortController = undefined;

    const restore = new Map<string, CheckpointBaseline>();
    for (let i = idx; i < this.history.length; i++) {
      const e = this.history[i];
      if (e.kind === "checkpoint" && e.baselines) {
        for (const b of e.baselines) {
          if (!restore.has(b.path)) {restore.set(b.path, b);}
        }
      }
    }

    const folder = vscode.workspace.workspaceFolders?.[0];
    for (const [path, b] of restore) {
      if (folder) {
        const uri = vscode.Uri.joinPath(folder.uri, path);
        try {
          if (b.existed) {
            await vscode.workspace.fs.writeFile(uri, Buffer.from(b.before, "utf8"));
          } else {
            await vscode.workspace.fs.delete(uri, { useTrash: true });
          }
        } catch {
          /* file may have been moved or already removed */
        }
      }
      this.editContents.delete(path);
      this.fileEdits.delete(path);
      this.editReview.dropEdit(path);
    }

    this.history = this.history.slice(0, idx);
    this.currentTurnId = undefined;
    this.saveHistory();
    this.post({ type: "clearMessages" });
    this.restoreHistory();
  }

  /** Syncs chat card, maps, and persistence after a per-block accept/reject in the editor. */
  private syncEditAfterBlockOp(path: string, originalOld: string, latestNew: string) {
    this.editContents.set(path, { originalOld, latestNew });
    if (this.fileEdits.has(path)) {
      this.fileEdits.set(path, { path, originalOld, latestNew });
    }
    const entry = this.history.find((e) => e.kind === "fileEdit" && e.path === path);
    if (entry) {
      entry.editOld = originalOld;
      entry.editNew = latestNew;
      this.saveHistory();
    }
    const { additions, deletions } = diffStat(originalOld, latestNew);
    this.post({
      type: "fileEdit",
      path,
      additions,
      deletions,
      tooLarge: false,
      isNew: originalOld.length === 0 && latestNew.length > 0,
      diff: computeDiffHunks(originalOld, latestNew),
    });
  }

  /** Records the accept/reject outcome of an edit so its state persists across reloads. */
  private markEditResolved(path: string, action: "accept" | "reject") {
    const entry = this.history.find((e) => e.kind === "fileEdit" && e.path === path);
    if (entry && entry.editStatus !== action) {
      entry.editStatus = action;
      this.saveHistory();
    }
  }

  /** Opens the just-edited file (preview tab, focus stays in chat) when enabled. */
  private autoOpenEdit(relPath: string) {
    const config = vscode.workspace.getConfiguration("getaibd");
    if (!config.get<boolean>("editReview.autoOpen", true)) {return;}
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) {return;}
    const uri = vscode.Uri.joinPath(folder.uri, relPath);
    void vscode.workspace.openTextDocument(uri).then(
      (doc) =>
        vscode.window.showTextDocument(doc, {
          preview: true,
          preserveFocus: true,
          viewColumn: vscode.ViewColumn.One,
        }),
      () => undefined,
    );
  }

  /** Re-applies a persisted agent edit to the in-editor review UI and chat card. */
  private restoreFileEdit(entry: HistoryEntry) {
    const path = entry.path;
    if (!path) {return;}
    const originalOld = entry.editOld ?? "";
    const latestNew = entry.editNew ?? "";
    const tooLarge = !!entry.tooLarge;
    const status = entry.editStatus ?? "pending";
    this.editContents.set(path, { originalOld, latestNew });
    if (status === "pending") {
      this.fileEdits.set(path, { path, originalOld, latestNew });
      this.editReview.addEdit(path, originalOld, latestNew);
    }
    const { additions, deletions } = diffStat(originalOld, latestNew);
    const diff = tooLarge ? { lines: [], truncated: true } : computeDiffHunks(originalOld, latestNew);
    this.post({
      type: "fileEdit",
      path,
      additions,
      deletions,
      tooLarge,
      isNew: originalOld.length === 0 && latestNew.length > 0,
      diff,
    });
    if (status !== "pending") {
      this.post({ type: "editResolved", path, action: status });
    }
  }

  /** Opens an agent edit as a native VS Code diff (original vs current file). */
  private async openDiff(filePath: string) {
    const edit = this.editContents.get(filePath);
    if (!edit) {return;}
    this.lastDiffPath = filePath;
    const q = encodeURIComponent(filePath);
    const leftUri = vscode.Uri.parse(`getaibd-diff:/${filePath}?${q}`);
    const folder = vscode.workspace.workspaceFolders?.[0];
    const fileUri = folder ? vscode.Uri.joinPath(folder.uri, filePath) : undefined;
    let rightUri = vscode.Uri.parse(`getaibd-diff-new:/${filePath}?${q}`);
    if (fileUri) {
      try {
        await vscode.workspace.fs.stat(fileUri);
        rightUri = fileUri;
      } catch {
        /* file removed; fall back to the captured new content */
      }
    }
    await vscode.commands.executeCommand(
      "vscode.diff",
      leftUri,
      rightUri,
      `${filePath} (agent edit)`,
    );
    this.updateDiffContext();
  }

  /** Toggles the getaibd.diffOpen context key based on whether the active tab is our diff. */
  updateDiffContext() {
    const tab = vscode.window.tabGroups.activeTabGroup?.activeTab;
    const input = tab?.input;
    const isDiff =
      input instanceof vscode.TabInputTextDiff && input.original?.scheme === "getaibd-diff";
    void vscode.commands.executeCommand("setContext", "getaibd.diffOpen", isDiff);
  }

  /** Accepts all pending agent edits (no-op on disk; just clears the review state). */
  private keepEdits() {
    for (const path of [...this.fileEdits.keys()]) {
      this.editReview.accept(path);
    }
    this.fileEdits.clear();
  }

  /** Accepts a single pending edit, leaving the file as written. */
  private keepEdit(filePath: string) {
    this.editReview.accept(filePath);
    this.fileEdits.delete(filePath);
  }

  /** Reverts every pending agent edit back to its original content. */
  private async undoEdits() {
    for (const edit of [...this.fileEdits.values()]) {
      await this.editReview.reject(edit.path);
    }
    this.fileEdits.clear();
  }

  /** Reverts a single pending edit back to its original content. */
  private async undoEdit(filePath: string) {
    await this.editReview.reject(filePath);
    this.fileEdits.delete(filePath);
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

  /** Tools the user marked "Always allow", which run without prompting. */
  private getAlwaysAllow(): string[] {
    return this.globalState.get<string[]>(ALWAYS_ALLOW_KEY, []);
  }

  private handleApprovalRequest(requestId: string, sessionId: string | undefined, toolName: string, args: Record<string, unknown>) {
    this.pendingApprovals.set(requestId, sessionId);
    this.pendingApprovalTools.set(requestId, toolName);
    if (this.getAlwaysAllow().includes(toolName)) {
      void this.resolveApproval(requestId, true);
      return;
    }
    this.post({ type: "approvalRequest", requestId, toolName, args });
  }

  /** Resolves an inline approval card click by notifying the engine gate. */
  private async resolveApproval(requestId: string, approved: boolean, always = false) {
    const sessionId = this.pendingApprovals.get(requestId);
    const toolName = this.pendingApprovalTools.get(requestId);
    this.pendingApprovals.delete(requestId);
    this.pendingApprovalTools.delete(requestId);
    if (always && approved && toolName) {
      const list = this.getAlwaysAllow();
      if (!list.includes(toolName)) {
        list.push(toolName);
        await this.globalState.update(ALWAYS_ALLOW_KEY, list);
      }
    }
    await sendApproval(requestId, approved, sessionId);
  }

  /** Runs an agent shell command in the managed terminal and returns the result to the engine. */
  private async handleTerminalExec(
    requestId: string,
    sessionId: string | undefined,
    args: Record<string, unknown>,
  ) {
    const command = typeof args.command === "string" ? args.command : "";
    const cmdArgs = Array.isArray(args.args)
      ? args.args.filter((a): a is string => typeof a === "string")
      : [];
    const cwd = typeof args.cwd === "string" ? args.cwd : undefined;
    this.activeTerminalReq = { requestId, sessionId };
    let result = { stdout: "", stderr: "", exit_code: 0 };
    try {
      result = await this.agentTerminal.run(command, cmdArgs, cwd, () => undefined);
    } catch (err) {
      result = { stdout: "", stderr: err instanceof Error ? err.message : String(err), exit_code: 1 };
    }
    this.activeTerminalReq = undefined;
    await sendTerminalResult(requestId, result, sessionId);
  }

  /** Surfaces a clarifying question with options as an interactive card in the chat. */
  private handleAskRequest(
    requestId: string,
    sessionId: string | undefined,
    question: string,
    options: string[],
    multiple: boolean,
  ) {
    this.pendingAsks.set(requestId, sessionId);
    this.post({ type: "askRequest", requestId, question, options, multiple });
  }

  /** Sends the user's answer for a clarifying question back to the engine gate. */
  private async resolveAsk(requestId: string, answer: string) {
    const sessionId = this.pendingAsks.get(requestId);
    this.pendingAsks.delete(requestId);
    await sendAskResult(requestId, answer, sessionId);
  }

  /** Cancels any in-flight terminal command and unblocks the engine. */
  private cancelActiveTerminal() {
    const req = this.activeTerminalReq;
    if (!req) {return;}
    this.activeTerminalReq = undefined;
    this.agentTerminal.cancel();
    void sendTerminalResult(
      req.requestId,
      { stdout: "", stderr: "Cancelled by user", exit_code: 130 },
      req.sessionId,
    );
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
    this.editContents.clear();
    this.fileEdits.clear();
    this.editReview.clearAll();
    for (const entry of this.history) {
      if (entry.kind === "context") {
        this.post({ type: "addContext", label: entry.label, code: entry.content });
      } else if (entry.kind === "fileEdit") {
        this.restoreFileEdit(entry);
      } else if (entry.kind === "checkpoint") {
        /* metadata only; the user message carries the restore control */
      } else {
        this.post({
          type: "addMessage",
          role: entry.role,
          content: entry.content,
          turnId: entry.turnId,
          canRestore: !!entry.turnId && entry.role === "user",
        });
      }
    }
  }

  private saveHistory() {
    this.sessionStore.update(SESSION_HISTORY_PREFIX + this.activeSessionId, this.history);
    this.maybeTitleFromHistory();
  }

  /** Prior user/assistant turns, most-recent-first within a char budget. The engine
   * further trims to the model's context window, so sizing is handled automatically. */
  private conversationMessages(): ChatMessage[] {
    const all: ChatMessage[] = [];
    for (const e of this.history) {
      if (e.kind !== "message" || !e.content) {continue;}
      if (e.role !== "user" && e.role !== "assistant") {continue;}
      all.push({ role: e.role, content: e.content });
    }
    const BUDGET = HISTORY_CHAR_BUDGET;
    const out: ChatMessage[] = [];
    let used = 0;
    for (let i = all.length - 1; i >= 0; i--) {
      used += all[i].content.length;
      if (used > BUDGET && out.length >= 2) {break;}
      out.unshift(all[i]);
    }
    return out;
  }

  /** Loads this workspace's session metadata (each project keeps its own). */
  private loadSessions() {
    this.sessions = this.sessionStore.get<SessionMeta[]>(SESSIONS_KEY, []);
    if (this.sessions.length === 0) {
      const first: SessionMeta = { id: newId(), title: "New chat", createdAt: Date.now() };
      this.sessions = [first];
      this.activeSessionId = first.id;
      this.sessionStore.update(SESSIONS_KEY, this.sessions);
      this.sessionStore.update(ACTIVE_SESSION_KEY, first.id);
      this.sessionStore.update(SESSION_HISTORY_PREFIX + first.id, []);
    } else {
      this.activeSessionId = this.sessionStore.get<string>(ACTIVE_SESSION_KEY, this.sessions[0].id);
      if (!this.sessions.some((s) => s.id === this.activeSessionId)) {
        this.activeSessionId = this.sessions[0].id;
      }
    }
    this.history = this.sessionStore.get<HistoryEntry[]>(
      SESSION_HISTORY_PREFIX + this.activeSessionId,
      [],
    );
  }

  private saveSessions() {
    this.sessionStore.update(SESSIONS_KEY, this.sessions);
    this.sessionStore.update(ACTIVE_SESSION_KEY, this.activeSessionId);
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
    this.currentTurnId = undefined;
    this.resetContextInjectionState();
    this.editContents.clear();
    this.fileEdits.clear();
    this.editReview.clearAll();
    this.sessionStore.update(SESSION_HISTORY_PREFIX + session.id, []);
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
    this.currentTurnId = undefined;
    this.activeSessionId = id;
    this.history = this.sessionStore.get<HistoryEntry[]>(SESSION_HISTORY_PREFIX + id, []);
    this.resetContextInjectionState();
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
    this.sessionStore.update(SESSION_HISTORY_PREFIX + id, undefined);
    if (this.sessions.length === 0) {
      const session: SessionMeta = { id: newId(), title: "New chat", createdAt: Date.now() };
      this.sessions = [session];
    }
    if (id === this.activeSessionId) {
      this.activeSessionId = this.sessions[0].id;
      this.history = this.sessionStore.get<HistoryEntry[]>(
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
    this.agentTerminal.dispose();
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

/** Approximate per-line additions/deletions for an edit badge. */
function diffStat(oldText: string, newText: string): { additions: number; deletions: number } {
  const tally = (s: string): Map<string, number> => {
    const m = new Map<string, number>();
    for (const line of s.split("\n")) {m.set(line, (m.get(line) ?? 0) + 1);}
    return m;
  };
  const a = tally(oldText);
  const b = tally(newText);
  let additions = 0;
  let deletions = 0;
  for (const [line, c] of b) {additions += Math.max(0, c - (a.get(line) ?? 0));}
  for (const [line, c] of a) {deletions += Math.max(0, c - (b.get(line) ?? 0));}
  return { additions, deletions };
}

interface DiffLine { t: string; s: string }
interface DiffHunks { lines: DiffLine[]; truncated: boolean }

/** Line-level diff (LCS) condensed to a few context lines for an inline preview. */
function computeDiffHunks(oldText: string, newText: string, maxLines = 240): DiffHunks {
  const a = oldText.split("\n");
  const b = newText.split("\n");
  const n = a.length;
  const m = b.length;
  if (n * m > 4_000_000) {
    return { lines: [], truncated: true };
  }
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { out.push({ t: " ", s: a[i] }); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) { out.push({ t: "-", s: a[i] }); i++; }
    else { out.push({ t: "+", s: b[j] }); j++; }
  }
  while (i < n) { out.push({ t: "-", s: a[i] }); i++; }
  while (j < m) { out.push({ t: "+", s: b[j] }); j++; }
  const condensed = condenseContext(out, 3);
  const truncated = condensed.length > maxLines;
  return { lines: truncated ? condensed.slice(0, maxLines) : condensed, truncated };
}

/** Keeps `ctx` context lines around each change; collapses the rest into gap markers. */
function condenseContext(lines: DiffLine[], ctx: number): DiffLine[] {
  const keep = new Array(lines.length).fill(false);
  for (let k = 0; k < lines.length; k++) {
    if (lines[k].t !== " ") {
      for (let d = -ctx; d <= ctx; d++) {
        const idx = k + d;
        if (idx >= 0 && idx < lines.length) {keep[idx] = true;}
      }
    }
  }
  const out: DiffLine[] = [];
  let gap = false;
  for (let k = 0; k < lines.length; k++) {
    if (keep[k]) { out.push(lines[k]); gap = false; }
    else if (!gap) { out.push({ t: "@", s: "" }); gap = true; }
  }
  return out;
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
  padding: 10px 14px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.message {
  padding: 4px 2px;
  border-radius: 8px;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
  position: relative;
  font-size: 13px;
}

.message.user {
  background: var(--user-bg);
  align-self: flex-end;
  max-width: 88%;
  padding: 7px 12px;
  border-bottom-right-radius: 2px;
  margin-top: 6px;
}

.message.assistant {
  align-self: stretch;
  max-width: 100%;
  padding: 2px 56px 2px 2px;
}

.message .role-label {
  display: none;
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

/* Per-block copy header injected around fenced code blocks. */
.code-block {
  margin: 0 0 8px;
  border: 1px solid var(--border);
  border-radius: 6px;
  overflow: hidden;
}
.code-block-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 2px 6px 2px 10px;
  background: var(--list-hover);
  border-bottom: 1px solid var(--border);
}
.code-block-lang { font-size: 10px; color: var(--muted); text-transform: lowercase; }
.code-copy-btn {
  background: var(--input-bg);
  border: 1px solid var(--border);
  color: var(--muted);
  border-radius: 4px;
  padding: 1px 8px;
  cursor: pointer;
  font-size: 10px;
  line-height: 16px;
}
.code-copy-btn:hover { color: var(--fg); border-color: var(--fg); }
.md .code-block pre { margin: 0; border: none; border-radius: 0; }

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
.btn-restore { right: 6px; }

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
.ctx-estimate { margin-left: auto; font-size: 11px; color: var(--muted); opacity: 0.85; }
@keyframes spin { to { transform: rotate(360deg); } }

/* ── Boot overlay (shown until the engine + providers are ready) ── */
.boot-overlay { position: fixed; inset: 0; z-index: 100; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 14px; background: var(--bg); }
.boot-overlay.hidden { display: none; }
.boot-spinner { width: 30px; height: 30px; border: 3px solid var(--muted); border-top-color: transparent; border-radius: 50%; animation: spin 0.8s linear infinite; }
.boot-text { color: var(--muted); font-size: 13px; max-width: 80%; text-align: center; line-height: 1.5; }
.boot-overlay.error .boot-spinner { display: none; }
.boot-overlay.error .boot-text { color: var(--error-fg); }

.reconnect-banner { display: none; padding: 6px 14px; background: var(--warn-bg); color: var(--fg); font-size: 12px; text-align: center; border-radius: 4px; margin: 0 12px; }
.reconnect-banner.visible { display: block; }

.tool-call { background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px; padding: 8px 12px; font-size: 12px; margin: 2px 0; }
.tool-call .tool-name { font-weight: 600; color: var(--btn-bg); font-size: 12px; }
.tool-call .tool-args { font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; color: var(--muted); white-space: pre-wrap; max-height: 120px; overflow-y: auto; margin-top: 4px; }
.tool-result { background: var(--code-bg); border-left: 3px solid var(--btn-bg); border-radius: 0 6px 6px 0; padding: 6px 12px; font-size: 11px; font-family: var(--vscode-editor-font-family, monospace); white-space: pre-wrap; max-height: 200px; overflow-y: auto; color: var(--muted); margin: 2px 0; }
.agent-status { padding: 4px 14px; font-size: 11px; color: var(--muted); font-style: italic; }
.continue-card { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; padding: 8px 14px 12px; }
.continue-note { font-size: 11px; color: var(--muted); }
.continue-btn { padding: 5px 14px; font-size: 12px; font-weight: 600; border: none; border-radius: 6px; cursor: pointer; background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
.continue-btn:hover:not(:disabled) { background: var(--vscode-button-hoverBackground, var(--vscode-button-background)); }
.continue-btn:disabled { opacity: 0.5; cursor: default; }
.file-edit { background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px; margin: 4px 0; font-size: 12px; overflow: hidden; }
.file-edit.fe-accepted { border-color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); opacity: 0.75; }
.file-edit.fe-reverted { opacity: 0.5; }
.file-edit.fe-reverted .fe-path { text-decoration: line-through; }
.file-edit.fe-accepted .fe-actions, .file-edit.fe-reverted .fe-actions { display: none; }
.fe-head { display: flex; align-items: center; gap: 8px; padding: 6px 10px; cursor: pointer; }
.fe-head:hover { background: var(--vscode-list-hoverBackground, transparent); }
.fe-icon { opacity: 0.8; }
.fe-path { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--vscode-editor-font-family, monospace); cursor: pointer; }
.fe-path:hover { text-decoration: underline; }
.fe-badge { font-size: 10px; text-transform: uppercase; letter-spacing: 0.04em; padding: 1px 6px; border-radius: 4px; background: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); color: #fff; opacity: 0.85; }
.fe-stat { font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; }
.fe-stat .add { color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); }
.fe-stat .del { color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }
.fe-toggle { background: transparent; border: none; color: var(--muted); cursor: pointer; font-size: 12px; padding: 0 2px; }
.fe-actions { display: flex; gap: 4px; }
.fe-actions button { font-size: 11px; padding: 2px 10px; border-radius: 4px; border: 1px solid var(--border); cursor: pointer; background: transparent; color: var(--fg); }
.fe-accept { background: var(--btn-bg) !important; color: var(--btn-fg) !important; border-color: var(--btn-bg) !important; }
.fe-accept:hover { opacity: 0.9; }
.fe-revert:hover { border-color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }
.fe-diff { border-top: 1px solid var(--border); max-height: 320px; overflow: auto; font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; line-height: 1.45; padding: 4px 0; }
.fe-line { white-space: pre; padding: 0 8px; }
.fe-line .fe-sign { display: inline-block; width: 1ch; margin-right: 6px; opacity: 0.7; }
.fe-line.add { background: rgba(76,175,80,0.13); }
.fe-line.del { background: rgba(244,67,54,0.13); }
.fe-line.ctx { color: var(--muted); }
.fe-line.gap { color: var(--muted); opacity: 0.6; text-align: center; }

.tool-group { margin: 4px 0; }
.tool-group > .tg-summary { cursor: pointer; list-style: none; font-size: 12px; color: var(--muted); padding: 3px 2px; user-select: none; }
.tool-group > .tg-summary::-webkit-details-marker { display: none; }
.tool-group > .tg-summary::before { content: "\\25B8"; display: inline-block; margin-right: 6px; transition: transform 0.15s; opacity: 0.7; }
.tool-group[open] > .tg-summary::before { transform: rotate(90deg); }
.tool-group > .tg-summary:hover { color: var(--fg); }
.tg-body { padding: 2px 0 2px 14px; border-left: 1px solid var(--border); margin-left: 4px; }
.tool-row { display: flex; align-items: center; gap: 6px; padding: 2px 2px; font-size: 12px; color: var(--muted); margin: 1px 0; }
.tool-row .tool-verb { color: var(--fg); opacity: 0.85; }
.tool-row .tool-arg { font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; color: var(--muted); background: var(--code-bg); border-radius: 4px; padding: 1px 6px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 100%; }
.tool-row .tool-state { display: inline-block; width: 12px; height: 12px; flex: 0 0 12px; position: relative; }
.tool-row.pending .tool-state { border: 1.5px solid var(--border); border-top-color: var(--btn-bg); border-radius: 50%; animation: tg-spin 0.7s linear infinite; }
.tool-row.done .tool-state::before { content: "\\2713"; color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); font-size: 11px; position: absolute; top: -2px; left: 0; }
.tool-row.failed .tool-state::before { content: "\\2715"; color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); font-size: 11px; position: absolute; top: -2px; left: 0; }
@keyframes tg-spin { to { transform: rotate(360deg); } }
.tool-error { border-left-color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }

.approval-card { background: var(--code-bg); border: 1px solid var(--vscode-editorWarning-foreground, #cca700); border-radius: 6px; padding: 8px 10px; margin: 6px 0; font-size: 12px; }
.approval-card .ap-head { display: flex; align-items: center; gap: 6px; }
.approval-card .ap-icon { color: var(--vscode-editorWarning-foreground, #cca700); }
.approval-card .ap-detail { margin: 6px 0; }
.approval-card .ap-arg { font-family: var(--vscode-editor-font-family, monospace); font-size: 11px; background: var(--vscode-editor-background, var(--code-bg)); border: 1px solid var(--border); border-radius: 4px; padding: 4px 8px; display: block; white-space: pre-wrap; word-break: break-all; }
.approval-card .ap-actions { display: flex; gap: 6px; margin-top: 8px; }
.approval-card button { font-size: 11px; padding: 3px 14px; border-radius: 4px; border: 1px solid var(--border); cursor: pointer; background: transparent; color: var(--fg); }
.approval-card .ap-allow { background: var(--btn-bg); color: var(--btn-fg); border-color: var(--btn-bg); }
.approval-card .ap-allow:hover { opacity: 0.9; }
.approval-card .ap-always:hover { border-color: var(--btn-bg); color: var(--btn-bg); }
.approval-card .ap-deny:hover { border-color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }
.approval-card .ap-result { color: var(--muted); font-style: italic; }
.approval-card.ap-allowed { border-color: var(--border); opacity: 0.8; }
.approval-card.ap-denied { border-color: var(--border); opacity: 0.6; }

.ask-card { border: 1px solid var(--border); border-radius: 8px; padding: 10px 12px; margin: 8px 0; background: var(--vscode-editorWidget-background, var(--code-bg)); }
.ask-card .ask-head { display: flex; gap: 8px; align-items: flex-start; }
.ask-card .ask-icon { display: inline-flex; align-items: center; justify-content: center; width: 18px; height: 18px; border-radius: 50%; background: var(--btn-bg); color: var(--btn-fg); font-size: 12px; font-weight: 700; flex-shrink: 0; margin-top: 1px; }
.ask-card .ask-q { font-weight: 600; line-height: 1.4; }
.ask-card .ask-opts { display: flex; flex-direction: column; gap: 6px; margin: 10px 0 8px; }
.ask-card .ask-opt { display: flex; align-items: center; gap: 8px; text-align: left; width: 100%; padding: 7px 10px; border-radius: 6px; border: 1px solid var(--border); background: transparent; color: var(--fg); cursor: pointer; font-size: 12px; }
.ask-card .ask-opt:hover { border-color: var(--btn-bg); }
.ask-card .ask-opt.sel { border-color: var(--btn-bg); background: var(--vscode-list-activeSelectionBackground, rgba(120,160,255,0.15)); }
.ask-card .ask-opt .ask-box { width: 12px; height: 12px; border: 1px solid var(--border); border-radius: 3px; flex-shrink: 0; }
.ask-card .ask-opt.sel .ask-box { background: var(--btn-bg); border-color: var(--btn-bg); }
.ask-card .ask-opt:disabled { opacity: 0.55; cursor: default; }
.ask-card .ask-other { margin: 6px 0; }
.ask-card .ask-input { width: 100%; padding: 6px 8px; border-radius: 6px; border: 1px solid var(--border); background: var(--vscode-input-background, var(--code-bg)); color: var(--fg); font-size: 12px; box-sizing: border-box; }
.ask-card .ask-actions { display: flex; justify-content: flex-end; margin-top: 4px; }
.ask-card .ask-send { font-size: 11px; padding: 4px 16px; border-radius: 4px; border: 1px solid var(--btn-bg); background: var(--btn-bg); color: var(--btn-fg); cursor: pointer; }
.ask-card .ask-result { color: var(--muted); font-style: italic; }
.ask-card.ask-done { opacity: 0.85; }

.md ul li.task-item { list-style: none; margin-left: -18px; display: flex; align-items: flex-start; gap: 6px; }
.md li.task-item .task-box { flex: 0 0 14px; width: 14px; height: 14px; border: 1px solid var(--border); border-radius: 3px; display: inline-flex; align-items: center; justify-content: center; font-size: 10px; line-height: 1; margin-top: 2px; color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); }
.md li.task-item.checked .task-box { background: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); color: #fff; border-color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); }
.md li.task-item.checked { opacity: 0.75; }

.edit-summary { display: flex; align-items: center; justify-content: space-between; gap: 8px; background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px; padding: 6px 10px; font-size: 12px; margin: 6px 0; }
.edit-summary .es-label { color: var(--fg); }
.edit-summary .es-label.es-done { color: var(--muted); font-style: italic; }
.edit-summary .add { color: var(--vscode-gitDecoration-addedResourceForeground, #4caf50); }
.edit-summary .del { color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }
.edit-summary .es-actions { display: flex; gap: 6px; }
.edit-summary button { font-size: 11px; padding: 3px 12px; border-radius: 4px; border: 1px solid var(--border); cursor: pointer; background: transparent; color: var(--fg); }
.edit-summary .es-keep { background: var(--btn-bg); color: var(--btn-fg); border-color: var(--btn-bg); }
.edit-summary .es-keep:hover { opacity: 0.9; }
.edit-summary .es-undo:hover { border-color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); color: var(--vscode-gitDecoration-deletedResourceForeground, #f44336); }

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

/* ── Composer ── */
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
.ctl-pill.model .pill-label { max-width: 140px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.cost-mode-switch {
  display: inline-flex;
  align-items: stretch;
  flex-shrink: 0;
  border: 1px solid var(--border);
  border-radius: 6px;
  overflow: hidden;
  background: var(--surface, var(--bg));
}
.header .cost-mode-switch {
  margin-right: 2px;
}
.header .cost-mode-option {
  font-size: 10px;
  padding: 4px 6px;
}
.cost-mode-option {
  border: 0;
  background: transparent;
  color: var(--muted);
  font-size: 11px;
  font-weight: 600;
  line-height: 1;
  padding: 6px 8px;
  cursor: pointer;
}
.cost-mode-option + .cost-mode-option { border-left: 1px solid var(--border); }
.cost-mode-option:hover { color: var(--text); background: var(--surface-2, rgba(255,255,255,0.06)); }
.cost-mode-option.active { background: var(--accent); color: #fff; }

.composer-row { display: flex; align-items: center; gap: 6px; }
.reasoning-row { flex-wrap: wrap; gap: 4px; padding-bottom: 2px; }
.reasoning-label { font-size: 11px; color: var(--muted); flex-shrink: 0; }
.reasoning-chips { display: flex; flex-wrap: wrap; gap: 4px; }
.reasoning-chip {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 10px;
  border: 1px solid var(--border);
  background: transparent;
  color: var(--muted);
  cursor: pointer;
  line-height: 1.4;
}
.reasoning-chip:hover { color: var(--fg); border-color: var(--fg); }
.reasoning-chip.active {
  border-color: #c084fc;
  color: #c084fc;
  background: rgba(192, 132, 252, 0.12);
}
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

.chat-only-badge { font-size: 10px; opacity: 0.75; cursor: help; line-height: 1; }

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

.setting-hint {
  font-size: 11px;
  color: var(--muted);
  margin: 0 0 8px;
  line-height: 1.4;
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

<div class="boot-overlay" id="bootOverlay">
  <div class="boot-spinner"></div>
  <div class="boot-text" id="bootText">Getting ready&hellip;</div>
</div>

<div id="diag" style="display:none;background:#5a1d1d;color:#fff;padding:6px 10px;font-size:11px;white-space:pre-wrap;line-height:1.4"></div>
<div class="header">
  <button class="icon-btn" id="newChatBtn" title="New chat">&#x2795;</button>
  <button class="icon-btn" id="sessionsBtn" title="Chat history">&#x1F551;</button>
  <span class="brand" id="brand">GetAIBD</span>
  <span class="header-spacer"></span>
  <button class="upgrade-btn" id="upgradeBtn" title="Add your API key to unlock all models" style="display:none">&#x1F511; Add API Key</button>
  <div class="cost-mode-switch" id="costModeSwitch" style="display:none" title="Reduced cost compresses tool context to save credits. This might degrade response.">
    <button type="button" class="cost-mode-option active" id="costNormalBtn">Normal</button>
    <button type="button" class="cost-mode-option" id="costReducedBtn" title="Reduced cost — compress tool context to save credits">Reduced</button>
  </div>
  <button class="icon-btn" id="settingsBtn" title="Settings">&#x2699;</button>
</div>
<div id="sessionsPanel" class="sessions-panel" style="display:none"></div>

<div class="messages" id="messages"></div>
<div class="reconnect-banner" id="reconnectBanner"></div>
<div class="spinner" id="spinner"><span id="spinnerLabel">Generating...</span><span class="ctx-estimate" id="ctxEstimate" style="display:none"></span></div>

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
  <div class="composer-row reasoning-row" id="reasoningRow" style="display:none" role="group" aria-label="Reasoning effort">
    <span class="reasoning-label">Reasoning</span>
    <div class="reasoning-chips" id="reasoningChips"></div>
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
const bootOverlay = document.getElementById("bootOverlay");
const bootText = document.getElementById("bootText");
const spinnerEl = document.getElementById("spinner");
const spinnerLabelEl = document.getElementById("spinnerLabel");
const ctxEstimateEl = document.getElementById("ctxEstimate");

function clearCtxEstimate() {
  if (ctxEstimateEl) {
    ctxEstimateEl.textContent = "";
    ctxEstimateEl.style.display = "none";
  }
}
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
const costModeSwitch = document.getElementById("costModeSwitch");
const costNormalBtn = document.getElementById("costNormalBtn");
const costReducedBtn = document.getElementById("costReducedBtn");
let compressEnabled = true;
if (upgradeBtn) {
  upgradeBtn.addEventListener("click", () => vscode.postMessage({ type: "needApiKey" }));
}

function updateCostModeUi() {
  const show = currentProvider === "getaibd" && !freeMode;
  if (costModeSwitch) { costModeSwitch.style.display = show ? "inline-flex" : "none"; }
  if (costNormalBtn) { costNormalBtn.classList.toggle("active", !compressEnabled); }
  if (costReducedBtn) { costReducedBtn.classList.toggle("active", compressEnabled); }
}

function setCompress(enabled) {
  if (compressEnabled === enabled) { return; }
  compressEnabled = enabled;
  updateCostModeUi();
  vscode.postMessage({ type: "compressChanged", compress: compressEnabled });
}

if (costNormalBtn) { costNormalBtn.addEventListener("click", () => setCompress(false)); }
if (costReducedBtn) { costReducedBtn.addEventListener("click", () => setCompress(true)); }

if (settingsPanel) {
  settingsPanel.addEventListener("click", (e) => {
    const t = e.target.closest("[data-act]");
    if (!t || t.tagName !== "BUTTON") { return; }
    const act = t.getAttribute("data-act");
    const arg = t.getAttribute("data-arg");
    if (act === "closeSettings") { closeSettings(); }
    else if (act === "toggleKeyVis") { toggleKeyVis(arg); }
    else if (act === "saveKey") { saveKey(arg); }
    else if (act === "removeKey") { removeKey(arg); }
    else if (act === "testProvider") { testProvider(arg); }
    else if (act === "removeAlwaysAllow") { vscode.postMessage({ type: "removeAlwaysAllow", tool: arg }); }
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
// The last streamed assistant bubble that could still be superseded by the
// completion reviewer. Unlike agentTextEl, this survives status events
// (planning/thinking/reflection) so agentDiscardDraft can always drop the
// right bubble and never leaves a duplicate "done" summary on screen.
let agentDraftEl = null;
let thoughtEl = null;
let thoughtBodyEl = null;
let thoughtStart = 0;
let currentMode = "agent";
let settingsOpen = false;
let editStats = {};
let editCardEls = {};
let editSummaryEl = null;
let toolGroupEl = null;
let toolGroupBodyEl = null;
let toolGroupCount = 0;
let pendingToolRows = [];

let currentProvider = "";
let currentModel = "";
let allModels = {};      // { providerId: [{id, name}] } from API
let allProviders = [];   // [{id, name}] from server
let curatedModels = {};  // { providerId: [{id, name, ctx, tags}] } from extension
let providerMeta = {};   // { providerId: { icon, color } }
let builtinProviders = []; // [{id, label}]
let activeProviderTab = "all";  // "all" or a provider id

let freeMode = false;
let needsPlan = false;
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

/* ── Mode switcher ── */
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

  const providerIds = getAllProviderIds().filter((pid) => {
    return (curatedModels[pid] || []).length > 0 || (allModels[pid] || []).length > 0;
  });
  if (providerIds.length <= 1) {
    providerTabsEl.style.display = "none";
    activeProviderTab = "all";
    return;
  }
  providerTabsEl.style.display = "";

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
      modelList.appendChild(makeModelItem(pid, m.id, m.name, m.ctx, m.tags || [], m.capabilities || []));
      total++;
    }

    if (apiModels.length > 0) {
      if (curated.length > 0) {
        const div = document.createElement("div");
        div.className = "model-divider";
        modelList.appendChild(div);
      }
      for (const m of apiModels) {
        modelList.appendChild(makeModelItem(pid, m.id, m.name || m.id, undefined, [], m.capabilities || []));
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

function makeModelItem(providerId, modelId, displayName, ctx, tags, capabilities) {
  const isFreeModel = modelId === freeModelId;
  const locked = needsPlan || (freeMode && !isFreeModel);
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

  const caps = capabilities || [];
  if (caps.length > 0 && !caps.includes("tools")) {
    const chatOnly = document.createElement("span");
    chatOnly.className = "chat-only-badge";
    chatOnly.textContent = "💬";
    chatOnly.title = "Chat only — this model can't use tools or run as an agent";
    badges.appendChild(chatOnly);
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
      vscode.postMessage({ type: needsPlan ? "needPlan" : "needApiKey" });
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
  updateCostModeUi();
  updateReasoningUi();
}

const REASONING_LEVELS = [
  { key: "off", label: "Off", title: "No extended reasoning — lowest cost" },
  { key: "low", label: "Low", title: "Light reasoning — cheaper" },
  { key: "medium", label: "Med", title: "Balanced reasoning (default)" },
  { key: "high", label: "High", title: "Deep reasoning — uses more credits" },
];
let currentReasoning = "medium";

function modelCaps(provider, modelId) {
  const api = (allModels[provider] || []).find(m => m.id === modelId);
  if (api && Array.isArray(api.capabilities)) return api.capabilities;
  const cur = (curatedModels[provider] || []).find(m => m.id === modelId);
  if (cur && Array.isArray(cur.capabilities)) return cur.capabilities;
  return [];
}

function currentModelSupportsThinking() {
  return modelCaps(currentProvider, currentModel).includes("thinking");
}

const reasoningRow = document.getElementById("reasoningRow");
const reasoningChips = document.getElementById("reasoningChips");

function buildReasoningChips() {
  if (!reasoningChips) { return; }
  reasoningChips.innerHTML = "";
  for (const r of REASONING_LEVELS) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "reasoning-chip";
    btn.dataset.key = r.key;
    btn.textContent = r.label;
    btn.title = r.title;
    btn.addEventListener("click", () => {
      currentReasoning = r.key;
      updateReasoningUi();
      vscode.postMessage({ type: "reasoningChanged", reasoningEffort: r.key });
    });
    reasoningChips.appendChild(btn);
  }
}

function updateReasoningUi() {
  if (!reasoningRow) { return; }
  const show = currentModelSupportsThinking();
  reasoningRow.style.display = show ? "flex" : "none";
  if (!show || !reasoningChips) { return; }
  reasoningChips.querySelectorAll(".reasoning-chip").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.key === currentReasoning);
  });
}

buildReasoningChips();

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

function detectPlanIntent(text) {
  const t = text.toLowerCase();
  return /\\b(plan|how (should|do|would|can) (i|we|you)|what(?:'s| is) the best way|best way to|approach to|strategy (for|to)|architecture (of|for)|design (a|an|the|for)|outline|steps to|break (this |it )?down|should (i|we))\\b/.test(t);
}

function send() {
  const text = inputEl.value.trim();
  if (!text) return;
  if (!currentProvider || !currentModel) {
    modelDropdown.classList.add("open");
    modelSearch.value = "";
    renderModelList("");
    return;
  }
  if (currentMode === "agent" && detectPlanIntent(text)) {
    switchMode("plan");
  }
  inputEl.value = "";
  inputEl.style.height = "auto";
  const reasoning = currentModelSupportsThinking() ? currentReasoning : "off";
  vscode.postMessage({
    type: "orchestratedSend",
    provider: currentProvider,
    model: currentModel,
    text,
    mode: currentMode,
    reasoningEffort: reasoning === "off" ? null : reasoning,
    compress: compressEnabled && currentProvider === "getaibd",
  });
}

/** Resumes a run that paused at the step limit, reusing the current selection. */
function continueRun() {
  if (!currentProvider || !currentModel) return;
  const reasoning = currentModelSupportsThinking() ? currentReasoning : "off";
  vscode.postMessage({
    type: "orchestratedSend",
    provider: currentProvider,
    model: currentModel,
    text: "Continue from where you left off and finish the task.",
    mode: currentMode,
    reasoningEffort: reasoning === "off" ? null : reasoning,
    compress: compressEnabled && currentProvider === "getaibd",
  });
}

/* ── Messages ── */
function addMessage(role, content, opts) {
  const div = document.createElement("div");
  div.className = "message " + role;
  const body = role === "assistant"
    ? '<div class="md">' + mdToHtml(content) + "</div>"
    : escapeHtml(content);
  div.innerHTML = '<span class="role-label">' + role + "</span>" + body;
  if (role === "assistant") enhanceCodeBlocks(div);
  if (role === "assistant" && content) appendActionBtns(div, content);
  if (opts && opts.canRestore && opts.turnId) appendRestoreBtn(div, opts.turnId);
  messagesEl.appendChild(div);
  scrollToBottom();
  return div;
}

function appendRestoreBtn(container, turnId) {
  const btn = document.createElement("button");
  btn.className = "msg-action-btn btn-restore";
  btn.title = "Revert file changes from this point and roll the chat back here";
  btn.textContent = "\u21ba Restore checkpoint";
  btn.addEventListener("click", () => {
    vscode.postMessage({ type: "restoreCheckpoint", turnId: turnId });
  });
  container.appendChild(btn);
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

/* Wrap each fenced code block in the rendered markdown with a header that has a
   per-block Copy button. Idempotent: skips blocks already wrapped. */
function enhanceCodeBlocks(root) {
  if (!root) return;
  const pres = root.querySelectorAll(".md pre");
  for (let i = 0; i < pres.length; i++) {
    const pre = pres[i];
    if (pre.parentElement && pre.parentElement.classList.contains("code-block")) continue;
    const codeEl = pre.querySelector("code");
    const codeText = codeEl ? codeEl.textContent : pre.textContent;
    let lang = "code";
    if (codeEl && codeEl.className) {
      const m = /language-([\w+#.-]+)/.exec(codeEl.className);
      if (m) lang = m[1];
    }
    const wrap = document.createElement("div");
    wrap.className = "code-block";
    const head = document.createElement("div");
    head.className = "code-block-head";
    const langSpan = document.createElement("span");
    langSpan.className = "code-block-lang";
    langSpan.textContent = lang;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "code-copy-btn";
    btn.textContent = "Copy";
    btn.addEventListener("click", () => {
      vscode.postMessage({ type: "copy", content: codeText });
      btn.textContent = "Copied";
      setTimeout(() => { btn.textContent = "Copy"; }, 1500);
    });
    head.appendChild(langSpan);
    head.appendChild(btn);
    pre.parentNode.insertBefore(wrap, pre);
    wrap.appendChild(head);
    wrap.appendChild(pre);
  }
}

function mdToHtml(text) {
  let html;
  if (typeof window.renderMarkdown === "function") {
    try { html = window.renderMarkdown(text || ""); } catch (e) { html = escapeHtml(text || ""); }
  } else {
    html = escapeHtml(text || "");
  }
  return enhanceTaskLists(html);
}

function enhanceTaskLists(html) {
  if (!html || html.indexOf("[") === -1) return html;
  return html.replace(/<li[^>]*>(\\s*<p>)?\\s*\\[([ xX])\\]\\s*/g, (m, p, c) => {
    const checked = c.toLowerCase() === "x";
    const cls = checked ? "task-item checked" : "task-item";
    return '<li class="' + cls + '">' + (p || "")
      + '<span class="task-box">' + (checked ? "\u2713" : "") + '</span>';
  });
}

function scrollToBottom() {
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function formatToolRow(name, args) {
  args = args || {};
  const path = args.path || args.file || args.file_path || args.filename;
  switch (name) {
    case "write_file":
    case "patch_file":
    case "apply_patch":
    case "edit_file":
      return { hidden: true };
    case "read_file":
      return { verb: "Read", arg: path };
    case "list_directory":
    case "list_dir":
      return { verb: "Listed", arg: path || args.dir || "." };
    case "search_files":
    case "grep":
    case "search":
      return { verb: "Searched", arg: args.query || args.pattern || path };
    case "run_command":
    case "run_terminal":
    case "shell": {
      const base = args.command || args.cmd || "";
      const extra = Array.isArray(args.args)
        ? args.args.join(" ")
        : (typeof args.args === "string" ? args.args : "");
      return { verb: "Ran", arg: (base + " " + extra).trim() || base };
    }
    default:
      if (name && name.indexOf("git") === 0) {
        const gitExtra = Array.isArray(args.args) ? args.args.join(" ") : (args.args || "");
        return { verb: name.replace(/_/g, " "), arg: gitExtra };
      }
      return { verb: (name || "tool").replace(/_/g, " "), arg: path };
  }
}

function ensureToolGroup() {
  if (!toolGroupEl) {
    toolGroupEl = document.createElement("details");
    toolGroupEl.className = "tool-group";
    toolGroupEl.open = true;
    const summary = document.createElement("summary");
    summary.className = "tg-summary";
    summary.textContent = "Working\u2026";
    toolGroupBodyEl = document.createElement("div");
    toolGroupBodyEl.className = "tg-body";
    toolGroupEl.appendChild(summary);
    toolGroupEl.appendChild(toolGroupBodyEl);
    messagesEl.appendChild(toolGroupEl);
    toolGroupCount = 0;
  }
  return toolGroupBodyEl;
}

function updateToolGroupSummary() {
  if (!toolGroupEl) return;
  const s = toolGroupEl.querySelector(".tg-summary");
  if (s) s.textContent = "Worked on " + toolGroupCount + " step" + (toolGroupCount === 1 ? "" : "s");
}

function closeToolGroup() {
  for (const row of pendingToolRows) {
    row.classList.remove("pending");
    row.classList.add("done");
  }
  pendingToolRows = [];
  if (toolGroupEl) {
    updateToolGroupSummary();
    toolGroupEl.open = false;
  }
  toolGroupEl = null;
  toolGroupBodyEl = null;
  toolGroupCount = 0;
}

function diffBodyHtml(diff) {
  let html = "";
  for (const ln of diff.lines) {
    if (ln.t === "@") { html += '<div class="fe-line gap">\u22EF</div>'; continue; }
    const cls = ln.t === "+" ? "add" : (ln.t === "-" ? "del" : "ctx");
    const sign = ln.t === " " ? "\u00A0" : ln.t;
    html += '<div class="fe-line ' + cls + '"><span class="fe-sign">' + sign + '</span>'
      + escapeHtml(ln.s || "") + '</div>';
  }
  if (diff.truncated) {
    html += '<div class="fe-line gap">\u22EF diff truncated \u2014 open in editor</div>';
  }
  return html;
}

function renderEditSummary() {
  const paths = Object.keys(editStats);
  if (paths.length === 0) {
    if (editSummaryEl) { editSummaryEl.style.display = "none"; editSummaryEl = null; }
    return;
  }
  if (!editSummaryEl) {
    editSummaryEl = document.createElement("div");
    editSummaryEl.className = "edit-summary";
    messagesEl.appendChild(editSummaryEl);
  }
  let a = 0, d = 0;
  for (const p of paths) { a += editStats[p].a; d += editStats[p].d; }
  const n = paths.length;
  const label = '<span class="es-label" title="Open diff">' + n + ' file' + (n === 1 ? '' : 's')
    + ' changed <span class="add">+' + a + '</span> <span class="del">-' + d + '</span></span>';
  const actions = '<span class="es-actions"><button class="es-keep">Accept all</button>'
    + '<button class="es-undo">Revert all</button></span>';
  editSummaryEl.innerHTML = label + actions;
  const labelEl = editSummaryEl.querySelector(".es-label");
  if (labelEl) {
    labelEl.style.cursor = "pointer";
    labelEl.addEventListener("click", () => vscode.postMessage({ type: "openDiff", path: paths[0] }));
  }
  editSummaryEl.querySelector(".es-keep").addEventListener("click", () => {
    vscode.postMessage({ type: "keepEdits" });
    markAllCards("fe-accepted");
    finalizeEdits("Accepted " + n + " file" + (n === 1 ? '' : 's'));
  });
  editSummaryEl.querySelector(".es-undo").addEventListener("click", () => {
    vscode.postMessage({ type: "undoEdits" });
    markAllCards("fe-reverted");
    finalizeEdits("Reverted " + n + " file" + (n === 1 ? '' : 's'));
  });
  messagesEl.appendChild(editSummaryEl);
}

function markAllCards(cls) {
  for (const key of Object.keys(editCardEls)) {
    const el = editCardEls[key];
    if (el) { el.classList.remove("fe-accepted", "fe-reverted"); el.classList.add(cls); }
  }
}

function finalizeEdits(text) {
  if (editSummaryEl) {
    editSummaryEl.innerHTML = '<span class="es-label es-done">' + escapeHtml(text) + '</span>';
    editSummaryEl.style.display = "";
  }
  editStats = {};
  editCardEls = {};
  editSummaryEl = null;
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
    .replace(/<plan>[\\s\\S]*?<\\/plan>/gi, "")
    .replace(/<thinking>[\\s\\S]*?<\\/thinking>/gi, "")
    .replace(/<reflection>[\\s\\S]*?<\\/reflection>/gi, "");
  out = out.replace(/<\\/?(plan|thinking|reflection)>/gi, "");
  const open = out.search(/<(plan|thinking|reflection)>[^]*$/i);
  if (open !== -1) out = out.slice(0, open);
  return out;
}

/* ── Settings Renderer ── */
function renderSettings(data) {
  let html = '<div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:12px;">'
    + '<span style="font-size:14px;font-weight:600;">Settings</span>'
    + '<button class="small-btn" data-act="closeSettings">Close</button></div>';

  html += '<div class="settings-section"><h3>Preferences</h3>';
  html += '<div class="pref-row"><label>Auto-attach open file</label>'
    + '<input type="checkbox" ' + (data.fileContextEnabled ? 'checked' : '') + ' data-act="savePref" data-arg="fileContext.enabled" /></div>';
  html += '<div class="pref-row"><label>Inline completions</label>'
    + '<input type="checkbox" ' + (data.inlineCompletionsEnabled ? 'checked' : '') + ' data-act="savePref" data-arg="inlineCompletions.enabled" /></div>';
  html += '<div class="pref-row"><label>Auto-open edited files</label>'
    + '<input type="checkbox" ' + (data.autoOpenEdits ? 'checked' : '') + ' data-act="savePref" data-arg="editReview.autoOpen" /></div>';
  html += '</div>';

  for (const p of (data.providers || [])) {
    const ph = p.hasKey ? '********' : '';
    html += '<div class="settings-section"><h3>' + escapeHtml(p.label) + ' API Key</h3>';
    html += '<p class="setting-hint">Add your own ' + escapeHtml(p.label)
      + ' API key to use paid models, or remove it to fall back to the free tier.</p>';
    html += '<div class="setting-row">'
      + '<input type="password" id="key_' + escapeHtml(p.id) + '" value="' + ph + '" placeholder="Paste API key" autocomplete="off" />'
      + '<button class="small-btn" data-act="toggleKeyVis" data-arg="' + escapeHtml(p.id) + '">Show</button>'
      + '<button class="small-btn primary" data-act="saveKey" data-arg="' + escapeHtml(p.id) + '">Save</button>'
      + (p.hasKey ? '<button class="small-btn danger" data-act="removeKey" data-arg="' + escapeHtml(p.id) + '">Remove</button>' : '')
      + '</div>';
    html += '<div class="pref-row"><label>Status</label><span>' + (p.hasKey ? 'Key configured' : 'No key (free tier)') + '</span></div>';
    html += '</div>';
  }

  const allow = data.alwaysAllow || [];
  html += '<div class="settings-section"><h3>Auto-approved tools</h3>';
  if (allow.length === 0) {
    html += '<p class="setting-hint">Tools you mark "Always allow" run without asking. None yet.</p>';
  } else {
    html += '<p class="setting-hint">These tools run automatically without asking. Remove one to require approval again.</p>';
    for (const tool of allow) {
      html += '<div class="pref-row"><label>' + escapeHtml(tool) + '</label>'
        + '<button class="small-btn danger" data-act="removeAlwaysAllow" data-arg="' + escapeHtml(tool) + '">Remove</button></div>';
    }
  }
  html += '</div>';

  settingsPanel.innerHTML = html;
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

function removeKey(id) {
  vscode.postMessage({ type: "removeApiKey", providerId: id });
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
    case "bootStatus":
      if (bootOverlay) {
        if (msg.state === "ready") {
          bootOverlay.classList.add("hidden");
        } else {
          bootOverlay.classList.remove("hidden");
          bootOverlay.classList.toggle("error", msg.state === "error");
          if (bootText && msg.text) { bootText.textContent = msg.text; }
        }
      }
      break;

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
      if (msg.compress !== undefined) {
        compressEnabled = !!msg.compress;
        updateCostModeUi();
      }
      if (msg.reasoningEffort) {
        currentReasoning = msg.reasoningEffort;
      }
      updateReasoningUi();
      break;

    case "authMode":
      freeMode = !!msg.free;
      needsPlan = !freeMode && msg.hasPlan === false;
      if (msg.freeModelId) freeModelId = msg.freeModelId;
      if (msg.freeModelLabel) freeModelLabel = msg.freeModelLabel;
      if (freeMode) {
        currentProvider = "getaibd";
        currentModel = freeModelId;
      }
      if (upgradeBtn) upgradeBtn.style.display = freeMode ? "inline-block" : "none";
      updateModelPill();
      updateCostModeUi();
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
      agentDraftEl = null;
      agentTextContent = "";
      break;

    case "setAgentMode":
      switchMode("agent");
      break;

    case "modeDetected": {
      const detected = msg.mode || "ask";
      if (MODE_META[detected]) switchMode(detected);
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
      addMessage(msg.role, msg.content, { turnId: msg.turnId, canRestore: msg.canRestore });
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
        enhanceCodeBlocks(streamEl);
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
      clearCtxEstimate();
      reconnectBanner.classList.remove("visible");
      break;

    case "streamError": {
      streaming = false;
      streamEl = null;
      streamContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      clearCtxEstimate();
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
      agentDraftEl = null;
      agentTextContent = "";
      thoughtEl = null;
      thoughtBodyEl = null;
      thoughtStart = 0;
      editStats = {};
      editCardEls = {};
      editSummaryEl = null;
      toolGroupEl = null;
      toolGroupBodyEl = null;
      toolGroupCount = 0;
      pendingToolRows = [];
      sendBtn.innerHTML = "&#9632;";
      sendBtn.classList.add("stop");
      spinnerLabelEl.textContent = "Agent working...";
      spinnerEl.classList.add("visible");
      break;

    case "contextEstimate":
      if (ctxEstimateEl) {
        const label = msg.label || "";
        ctxEstimateEl.textContent = label;
        ctxEstimateEl.style.display = label ? "inline" : "none";
        ctxEstimateEl.title =
          "Approximate client context in this request (history + attached files). " +
          "The engine adds RAG memory and tool output separately.";
      }
      break;

    case "agentToolCall": {
      agentTextEl = null;
      const info = formatToolRow(msg.name, msg.arguments);
      if (info.hidden) break;
      const body = ensureToolGroup();
      const row = document.createElement("div");
      row.className = "tool-row pending";
      let html = '<span class="tool-state"></span>';
      html += '<span class="tool-verb">' + escapeHtml(info.verb) + '</span>';
      if (info.arg) {
        html += ' <code class="tool-arg">' + escapeHtml(String(info.arg)) + '</code>';
      }
      row.innerHTML = html;
      body.appendChild(row);
      pendingToolRows.push(row);
      toolGroupCount++;
      updateToolGroupSummary();
      scrollToBottom();
      break;
    }

    case "agentToolResult": {
      let t = msg.result == null
        ? ""
        : (typeof msg.result === "string" ? msg.result : JSON.stringify(msg.result));
      const lower = t.slice(0, 80).toLowerCase();
      const isError = !!t && (lower.startsWith("error") || lower.indexOf('"error"') !== -1);
      const row = pendingToolRows.shift();
      if (row) {
        row.classList.remove("pending");
        row.classList.add(isError ? "failed" : "done");
      }
      if (isError && t) {
        const body = ensureToolGroup();
        const trDiv = document.createElement("div");
        trDiv.className = "tool-result tool-error";
        trDiv.textContent = t.length > 500 ? t.slice(0, 500) + "..." : t;
        body.appendChild(trDiv);
      }
      scrollToBottom();
      break;
    }

    case "fileEdit": {
      agentTextEl = null;
      closeToolGroup();
      const p = msg.path;
      let card = editCardEls[p];
      if (!card) {
        card = document.createElement("div");
        card.className = "file-edit";
        editCardEls[p] = card;
        messagesEl.appendChild(card);
      }
      card.classList.remove("fe-accepted", "fe-reverted");
      const statHtml = msg.tooLarge
        ? '<span class="fe-stat">large file</span>'
        : '<span class="fe-stat"><span class="add">+' + msg.additions + '</span> <span class="del">-' + msg.deletions + '</span></span>';
      const badge = msg.isNew ? '<span class="fe-badge">new</span>' : '';
      const hasDiff = msg.diff && msg.diff.lines && msg.diff.lines.length > 0;
      const toggle = hasDiff ? '<button class="fe-toggle" title="Toggle diff">\u25BE</button>' : '';
      const head = '<div class="fe-head">'
        + '<span class="fe-icon">\u270E</span>'
        + '<span class="fe-path" title="Open diff in editor">' + escapeHtml(p) + '</span>'
        + badge + statHtml + toggle
        + '<span class="fe-actions">'
        + '<button class="fe-accept">Accept</button>'
        + '<button class="fe-revert">Revert</button>'
        + '</span></div>';
      const body = hasDiff
        ? '<div class="fe-diff" style="display:none">' + diffBodyHtml(msg.diff) + '</div>'
        : '';
      card.innerHTML = head + body;
      const headEl = card.querySelector(".fe-head");
      if (headEl) headEl.addEventListener("click", (e) => {
        if (e.target && e.target.closest && e.target.closest("button")) return;
        vscode.postMessage({ type: "openDiff", path: p });
      });
      const toggleEl = card.querySelector(".fe-toggle");
      const diffEl = card.querySelector(".fe-diff");
      if (toggleEl && diffEl) toggleEl.addEventListener("click", () => {
        const shown = diffEl.style.display !== "none";
        diffEl.style.display = shown ? "none" : "block";
        toggleEl.textContent = shown ? "\u25BE" : "\u25B4";
      });
      const acceptEl = card.querySelector(".fe-accept");
      if (acceptEl) acceptEl.addEventListener("click", () => {
        vscode.postMessage({ type: "keepEdit", path: p });
        card.classList.add("fe-accepted");
        delete editStats[p];
        renderEditSummary();
      });
      const revertEl = card.querySelector(".fe-revert");
      if (revertEl) revertEl.addEventListener("click", () => {
        vscode.postMessage({ type: "undoEdit", path: p });
        card.classList.add("fe-reverted");
        delete editStats[p];
        renderEditSummary();
      });
      editStats[p] = msg.tooLarge ? { a: 0, d: 0 } : { a: msg.additions, d: msg.deletions };
      renderEditSummary();
      scrollToBottom();
      break;
    }

    case "editResolved": {
      const rp = msg.path;
      const rcard = editCardEls[rp];
      if (rcard) {
        rcard.classList.remove("fe-accepted", "fe-reverted");
        rcard.classList.add(msg.action === "reject" ? "fe-reverted" : "fe-accepted");
      }
      if (editStats[rp]) { delete editStats[rp]; renderEditSummary(); }
      break;
    }

    case "agentText": {
      if (!agentTextEl) {
        agentTextContent = "";
        closeToolGroup();
      }
      agentTextContent += msg.content || "";
      const clean = stripThinkingTags(agentTextContent);
      if (!clean.trim()) break;
      if (!agentTextEl) {
        agentTextEl = addMessage("assistant", "");
      }
      agentDraftEl = agentTextEl;
      agentTextEl.innerHTML = '<span class="role-label">assistant</span><div class="md">' + mdToHtml(clean) + "</div>";
      enhanceCodeBlocks(agentTextEl);
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

    case "agentDiscardDraft": {
      // A provisional "done" summary was superseded by more work — remove the
      // bubble that was just streamed so the user never sees a duplicate. Prefer
      // the tracked draft handle, which survives any status event (planning/
      // thinking/reflection) that may have detached agentTextEl in between.
      const draft = agentDraftEl || agentTextEl;
      if (draft) {
        draft.remove();
      }
      agentTextEl = null;
      agentDraftEl = null;
      agentTextContent = "";
      break;
    }

    case "agentDone":
      finalizeThought();
      closeToolGroup();
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
      agentDraftEl = null;
      agentTextContent = "";
      break;

    case "agentComplete":
      streaming = false;
      finalizeThought();
      closeToolGroup();
      if (agentTextEl) {
        const clean = stripThinkingTags(agentTextContent);
        if (clean.trim()) appendActionBtns(agentTextEl, clean);
      }
      agentTextEl = null;
      agentDraftEl = null;
      agentTextContent = "";
      sendBtn.innerHTML = "&#9654;";
      sendBtn.classList.remove("stop");
      spinnerEl.classList.remove("visible");
      clearCtxEstimate();
      {
        const cd = document.createElement("div");
        cd.className = "agent-status";
        cd.textContent = "Agent completed (" + msg.iterations + " iterations)";
        messagesEl.appendChild(cd);
        if (msg.stepLimit) {
          const cont = document.createElement("div");
          cont.className = "continue-card";
          cont.innerHTML = '<span class="continue-note">Paused at the step limit \u2014 the task may not be finished.</span>'
            + '<button class="continue-btn">Continue</button>';
          const btn = cont.querySelector(".continue-btn");
          btn.addEventListener("click", () => {
            btn.disabled = true;
            continueRun();
          });
          messagesEl.appendChild(cont);
        }
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
      clearCtxEstimate();
      {
        const ae = document.createElement("div");
        ae.className = "error-msg";
        ae.textContent = "Agent error: " + msg.error;
        messagesEl.appendChild(ae);
        scrollToBottom();
      }
      break;

    case "approvalRequest": {
      agentTextEl = null;
      const info = formatToolRow(msg.toolName, msg.args);
      const detail = info.arg ? '<code class="ap-arg">' + escapeHtml(String(info.arg)) + '</code>' : '';
      const card = document.createElement("div");
      card.className = "approval-card";
      card.innerHTML = '<div class="ap-head"><span class="ap-icon">\u26A0</span>'
        + '<span class="ap-title">Allow <b>' + escapeHtml(info.verb || msg.toolName) + '</b>?</span></div>'
        + (detail ? '<div class="ap-detail">' + detail + '</div>' : '')
        + '<div class="ap-actions"><button class="ap-allow">Allow</button>'
        + '<button class="ap-always">Always allow</button>'
        + '<button class="ap-deny">Deny</button></div>';
      messagesEl.appendChild(card);
      const finish = (approved, always, label) => {
        vscode.postMessage({ type: "approvalResponse", requestId: msg.requestId, approved, always });
        card.classList.add(approved ? "ap-allowed" : "ap-denied");
        card.querySelector(".ap-actions").innerHTML = '<span class="ap-result">' + label + '</span>';
      };
      card.querySelector(".ap-allow").addEventListener("click", () => finish(true, false, "Allowed"));
      card.querySelector(".ap-always").addEventListener("click", () => finish(true, true, "Always allowed"));
      card.querySelector(".ap-deny").addEventListener("click", () => finish(false, false, "Denied"));
      scrollToBottom();
      break;
    }

    case "askRequest": {
      agentTextEl = null;
      const opts = Array.isArray(msg.options) ? msg.options : [];
      const multiple = msg.multiple === true;
      const card = document.createElement("div");
      card.className = "ask-card";
      let optsHtml = "";
      for (let i = 0; i < opts.length; i++) {
        optsHtml += '<button class="ask-opt" data-i="' + i + '">'
          + (multiple ? '<span class="ask-box"></span>' : '')
          + '<span>' + escapeHtml(String(opts[i])) + '</span></button>';
      }
      card.innerHTML = '<div class="ask-head"><span class="ask-icon">?</span>'
        + '<span class="ask-q">' + escapeHtml(msg.question || "") + '</span></div>'
        + (optsHtml ? '<div class="ask-opts">' + optsHtml + '</div>' : '')
        + '<div class="ask-other"><input type="text" class="ask-input" placeholder="'
        + (opts.length ? 'Other\u2026' : 'Type your answer\u2026') + '" /></div>'
        + '<div class="ask-actions"><button class="ask-send">'
        + (multiple ? 'Send' : (opts.length ? 'Send' : 'Send')) + '</button></div>';
      messagesEl.appendChild(card);

      const selected = new Set();
      const input = card.querySelector(".ask-input");
      const submit = (answer) => {
        if (!answer.trim()) { return; }
        vscode.postMessage({ type: "askResponse", requestId: msg.requestId, answer: answer });
        card.classList.add("ask-done");
        card.querySelector(".ask-actions").innerHTML = '<span class="ask-result">' + escapeHtml(answer) + '</span>';
        const oi = card.querySelector(".ask-other");
        if (oi) { oi.remove(); }
        card.querySelectorAll(".ask-opt").forEach((b) => { b.disabled = true; });
      };

      card.querySelectorAll(".ask-opt").forEach((btn) => {
        btn.addEventListener("click", () => {
          const i = btn.getAttribute("data-i");
          const label = String(opts[i]);
          if (multiple) {
            if (selected.has(label)) { selected.delete(label); btn.classList.remove("sel"); }
            else { selected.add(label); btn.classList.add("sel"); }
          } else {
            submit(label);
          }
        });
      });
      card.querySelector(".ask-send").addEventListener("click", () => {
        const typed = input ? input.value.trim() : "";
        if (multiple) {
          const parts = Array.from(selected);
          if (typed) { parts.push(typed); }
          submit(parts.join(", "));
        } else {
          submit(typed);
        }
      });
      if (input) {
        input.addEventListener("keydown", (e) => {
          if (e.key === "Enter") { e.preventDefault(); card.querySelector(".ask-send").click(); }
        });
      }
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
