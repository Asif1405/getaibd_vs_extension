import * as vscode from "vscode";
import { ChatPanel } from "../chat/panel";

export function explainSelection(context: vscode.ExtensionContext) {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {return;}

  const selection = editor.selection;
  const selectedText = editor.document.getText(selection);
  if (!selectedText) {
    vscode.window.showWarningMessage("No text selected");
    return;
  }

  const panel = ChatPanel.open(context);
  panel.addContext("Explain this code", selectedText);

  const config = vscode.workspace.getConfiguration("getaibd");
  const provider = config.get<string>("lastProvider", "getaibd");
  const model = config.get<string>("lastModel", "") || config.get<string>("model", "");

  panel.streamToPanel(provider, model, [
    {
      role: "system",
      content: "Explain the following code clearly and concisely.",
    },
    { role: "user", content: selectedText },
  ]);
}
