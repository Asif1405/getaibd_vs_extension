import * as vscode from "vscode";
import { chat } from "../client";
import { ChatPanel } from "../chat/panel";

export async function completeSelection(context: vscode.ExtensionContext) {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {return;}

  const selection = editor.selection;
  const selectedText = editor.document.getText(selection);
  if (!selectedText) {
    vscode.window.showWarningMessage("No text selected");
    return;
  }

  const panel = ChatPanel.open(context);
  panel.addContext("Complete this code", selectedText);

  const pick = await pickProviderModel();
  if (!pick) {return;}

  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: "Completing..." },
    async () => {
      try {
        const response = await chat(pick.provider, pick.model, [
          {
            role: "system",
            content:
              "Complete the following code. Return ONLY the completed code, no explanation.",
          },
          { role: "user", content: selectedText },
        ]);

        await editor.edit((b) => b.replace(selection, response.content));
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : "Completion failed";
        vscode.window.showErrorMessage(message);
      }
    },
  );
}

async function pickProviderModel(): Promise<{ provider: string; model: string } | undefined> {
  const config = vscode.workspace.getConfiguration("getaibd");
  const provider = await vscode.window.showInputBox({
    prompt: "Provider ID",
    value: config.get<string>("chat.defaultProvider", "getaibd"),
  });
  if (!provider) {return undefined;}

  const model = await vscode.window.showInputBox({
    prompt: "Model ID (leave empty for provider default)",
    value: config.get<string>("model", ""),
  });
  if (model === undefined) {return undefined;}

  return { provider, model };
}
