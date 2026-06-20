import * as vscode from "vscode";
import { chat } from "../client";

const DEBOUNCE_MS = 500;
let timer: ReturnType<typeof setTimeout> | undefined;

export class InlineCompletionProvider implements vscode.InlineCompletionItemProvider {
  async provideInlineCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
    _ctx: vscode.InlineCompletionContext,
    token: vscode.CancellationToken,
  ): Promise<vscode.InlineCompletionItem[]> {
    const config = vscode.workspace.getConfiguration("getaibd");
    if (!config.get<boolean>("inlineCompletions.enabled", false)) {return [];}

    const provider = config.get<string>("inlineCompletions.provider", "getaibd");
    const model =
      config.get<string>("inlineCompletions.model", "") || config.get<string>("model", "");

    const prefix = document.getText(
      new vscode.Range(
        new vscode.Position(Math.max(0, position.line - 50), 0),
        position,
      ),
    );
    const suffix = document.getText(
      new vscode.Range(
        position,
        new vscode.Position(Math.min(document.lineCount, position.line + 10), 0),
      ),
    );

    if (prefix.trim().length < 5) {return [];}

    await new Promise<void>((resolve) => {
      if (timer) {clearTimeout(timer);}
      timer = setTimeout(resolve, DEBOUNCE_MS);
    });

    if (token.isCancellationRequested) {return [];}

    try {
      const response = await chat(provider, model, [
        {
          role: "system",
          content:
            "Complete the code at the cursor position. Return ONLY the completion text, nothing else. No markdown, no explanation.",
        },
        {
          role: "user",
          content: `${prefix}<CURSOR>${suffix}`,
        },
      ]);

      if (token.isCancellationRequested) {return [];}

      const completion = response.content.trim();
      if (!completion) {return [];}

      return [
        new vscode.InlineCompletionItem(
          completion,
          new vscode.Range(position, position),
        ),
      ];
    } catch {
      return [];
    }
  }
}
