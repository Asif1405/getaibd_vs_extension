import * as vscode from "vscode";
import { ChatPanel } from "./chat/panel";
import { completeSelection } from "./commands/complete";
import { explainSelection } from "./commands/explain";
import { generateTests } from "./commands/generateTests";
import { InlineCompletionProvider } from "./commands/inline";
import { activateDiagnostics } from "./safetype/diagnostics";
import { ensureEngine, restartEngine, stopEngine } from "./engine/manager";
import { getApiKey, setApiKey, isFreeToken } from "./util/config";
import { fetchAccountStatus } from "./client";
import { createFreeSession } from "./free";

const SIGNUP_URL = "https://getaibd.com";

export function activate(context: vscode.ExtensionContext) {
  activateDiagnostics(context);

  const openItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  openItem.text = "$(sparkle) GetAIBD";
  openItem.tooltip = "Open GetAIBD chat";
  openItem.command = "getaibd.openChat";
  openItem.show();
  context.subscriptions.push(openItem);

  const balanceItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  balanceItem.command = "getaibd.refreshBalance";
  balanceItem.tooltip = "GetAIBD credit balance (click to refresh)";
  context.subscriptions.push(balanceItem);

  const refreshBalance = async () => {
    const key = await getApiKey(context.secrets);
    if (!key) {
      balanceItem.hide();
      return;
    }
    const status = await fetchAccountStatus(key);
    if (!status) {
      balanceItem.hide();
      return;
    }
    if (status.free) {
      balanceItem.text = `$(rocket) Free: ${status.daysLeft ?? "?"}/${status.daysLimit ?? 3} days`;
      balanceItem.tooltip = "GetAIBD free tier — usage days left this month (click to refresh)";
    } else {
      balanceItem.text = `$(database) ${(status.creditsBalance ?? 0).toLocaleString()} credits`;
      balanceItem.tooltip = "GetAIBD credit balance (click to refresh)";
    }
    balanceItem.show();
  };

  /** Prompts for an API key, returns true once one is stored. */
  const promptApiKey = async (): Promise<boolean> => {
    const value = await vscode.window.showInputBox({
      title: "GetAIBD API Key",
      prompt: "Paste your GetAIBD API key (from https://getaibd.com)",
      placeHolder: "aiob_…",
      password: true,
      ignoreFocusOut: true,
      validateInput: (v) =>
        isFreeToken(v.trim())
          ? "That is an internal free-tier token. Paste your real GetAIBD API key."
          : undefined,
    });
    if (!value || !value.trim() || isFreeToken(value.trim())) {
      return false;
    }
    await setApiKey(context.secrets, value.trim());
    await restartEngine(context).catch(() => undefined);
    void refreshBalance();
    ChatPanel.current()?.refreshAuthMode();
    return true;
  };

  /** Starts an anonymous free-tier session, returns true on success. */
  const startFree = async (): Promise<boolean> => {
    try {
      const status = await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Notification, title: "Starting GetAIBD free session…" },
        () => createFreeSession(context),
      );
      await setApiKey(context.secrets, status.token);
      await restartEngine(context).catch(() => undefined);
      void refreshBalance();
      vscode.window.showInformationMessage(
        `GetAIBD free tier active — ${status.days_left}/${status.days_limit} usage days left this month. ` +
          `Using the free "Auto" model; add an API key any time for full model access.`,
      );
      return true;
    } catch (err: unknown) {
      vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "Could not start the free session.",
      );
      return false;
    }
  };

  /** First-run choice: bring your own key, or use the free tier. */
  const chooseAccess = async (): Promise<boolean> => {
    const pick = await vscode.window.showQuickPick(
      [
        {
          label: "$(key) Set API Key",
          detail: "Use your GetAIBD API key — full access to every model.",
          id: "key",
        },
        {
          label: "$(rocket) Use Free",
          detail: 'Free "Auto" model, 3 usage days per month. No key required.',
          id: "free",
        },
      ],
      {
        title: "GetAIBD — how would you like to start?",
        placeHolder: "Set an API key, or try the free tier",
        ignoreFocusOut: true,
      },
    );
    if (!pick) {
      return false;
    }
    return pick.id === "key" ? promptApiKey() : startFree();
  };

  /** Ensures the user has chosen a credential (key or free) before continuing. */
  const ensureChosen = async (): Promise<boolean> => {
    const key = await getApiKey(context.secrets);
    if (key) {
      return true;
    }
    return chooseAccess();
  };

  /** Onboards then starts the engine, surfacing a friendly error on failure. */
  const withEngine = (fn: () => void | Promise<void>) => async () => {
    if (!(await ensureChosen())) {
      return;
    }
    try {
      await ensureEngine(context);
      await fn();
      void refreshBalance();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Failed to start GetAIBD engine.";
      const choice = await vscode.window.showErrorMessage(message, "Set API Key", "Use Free");
      if (choice === "Set API Key") {
        await promptApiKey();
      } else if (choice === "Use Free") {
        await startFree();
      }
    }
  };

  context.subscriptions.push(
    ChatPanel.register(context),

    vscode.commands.registerCommand("getaibd.openChat", withEngine(() => {
      ChatPanel.open(context);
    })),

    vscode.commands.registerCommand("getaibd.newChat", withEngine(() => {
      ChatPanel.open(context);
      ChatPanel.current()?.startNewSession();
    })),

    vscode.commands.registerCommand("getaibd.useFree", async () => {
      if (await startFree()) {
        await ensureEngine(context).catch(() => undefined);
        ChatPanel.current()?.refreshAuthMode();
      }
    }),

    vscode.commands.registerCommand("getaibd.getApiKeyInfo", async () => {
      await vscode.env.openExternal(vscode.Uri.parse(SIGNUP_URL));
    }),

    vscode.commands.registerCommand("getaibd.openAgent", withEngine(() => {
      const panel = ChatPanel.open(context);
      panel.enableAgentMode();
    })),

    vscode.commands.registerCommand("getaibd.completeSelection", withEngine(() => {
      completeSelection(context);
    })),

    vscode.commands.registerCommand("getaibd.explainSelection", withEngine(() => {
      explainSelection(context);
    })),

    vscode.commands.registerCommand("getaibd.generateTests", withEngine(() => {
      generateTests(context);
    })),

    vscode.commands.registerCommand("getaibd.openSettings", withEngine(() => {
      const panel = ChatPanel.open(context);
      panel.openSettings();
    })),

    vscode.commands.registerCommand("getaibd.setApiKey", async () => {
      const existing = await getApiKey(context.secrets);
      const value = await vscode.window.showInputBox({
        title: "GetAIBD API Key",
        prompt: "Paste your GetAIBD API key (from https://getaibd.com)",
        password: true,
        ignoreFocusOut: true,
        value: isFreeToken(existing) ? "" : existing,
        validateInput: (v) =>
          isFreeToken(v.trim())
            ? "That is an internal free-tier token. Paste your real GetAIBD API key."
            : undefined,
      });
      if (value === undefined) {
        return;
      }
      const trimmed = value.trim();
      if (isFreeToken(trimmed)) {
        return;
      }
      await setApiKey(context.secrets, trimmed);
      await restartEngine(context).catch((err) => {
        vscode.window.showErrorMessage(
          err instanceof Error ? err.message : "Failed to restart GetAIBD engine.",
        );
      });
      void refreshBalance();
      ChatPanel.current()?.refreshAuthMode();
    }),

    vscode.commands.registerCommand("getaibd.refreshBalance", refreshBalance),

    vscode.commands.registerCommand("getaibd.restartEngine", async () => {
      try {
        await restartEngine(context);
        vscode.window.showInformationMessage("GetAIBD engine restarted.");
      } catch (err: unknown) {
        vscode.window.showErrorMessage(
          err instanceof Error ? err.message : "Failed to restart GetAIBD engine.",
        );
      }
    }),

    vscode.languages.registerInlineCompletionItemProvider(
      { pattern: "**" },
      new InlineCompletionProvider(),
    ),

    vscode.workspace.onDidChangeConfiguration((e) => {
      if (
        e.affectsConfiguration("getaibd.baseUrl") ||
        e.affectsConfiguration("getaibd.serverUrl") ||
        e.affectsConfiguration("getaibd.enginePath")
      ) {
        restartEngine(context).catch(() => undefined);
      }
    }),
  );

  void getApiKey(context.secrets).then((key) => {
    if (key) {
      ensureEngine(context).catch(() => undefined);
      void refreshBalance();
    }
  });
}

export function deactivate() {
  stopEngine();
}
