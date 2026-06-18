import * as vscode from "vscode";
import { ChatPanel } from "./chat/panel";
import { completeSelection } from "./commands/complete";
import { explainSelection } from "./commands/explain";
import { generateTests } from "./commands/generateTests";
import { InlineCompletionProvider } from "./commands/inline";
import { activateDiagnostics } from "./safetype/diagnostics";
import { ensureEngine, restartEngine, stopEngine } from "./engine/manager";
import { getApiKey, setApiKey } from "./util/config";
import { fetchPlatformBalance } from "./client";

export function activate(context: vscode.ExtensionContext) {
  activateDiagnostics(context);

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
    const balance = await fetchPlatformBalance(key);
    if (balance === null) {
      balanceItem.hide();
      return;
    }
    balanceItem.text = `$(database) ${balance.toLocaleString()} credits`;
    balanceItem.show();
  };

  /** Starts the engine on demand, surfacing a friendly error if the key is missing. */
  const withEngine = (fn: () => void | Promise<void>) => async () => {
    try {
      await ensureEngine(context);
      await fn();
      void refreshBalance();
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Failed to start GetAIBD engine.";
      const choice = await vscode.window.showErrorMessage(message, "Set API Key");
      if (choice === "Set API Key") {
        await vscode.commands.executeCommand("getaibd.setApiKey");
      }
    }
  };

  context.subscriptions.push(
    vscode.commands.registerCommand("getaibd.openChat", withEngine(() => {
      ChatPanel.open(context);
    })),

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
      const value = await vscode.window.showInputBox({
        title: "GetAIBD API Key",
        prompt: "Paste your GetAIBD API key (from https://getaibd.com)",
        password: true,
        ignoreFocusOut: true,
        value: await getApiKey(context.secrets),
      });
      if (value === undefined) {
        return;
      }
      await setApiKey(context.secrets, value);
      await restartEngine(context).catch((err) => {
        vscode.window.showErrorMessage(
          err instanceof Error ? err.message : "Failed to restart GetAIBD engine.",
        );
      });
      void refreshBalance();
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
