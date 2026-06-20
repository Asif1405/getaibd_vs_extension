import * as vscode from "vscode";
import { SECRET_RULES } from "./rules";

const COLLECTION = vscode.languages.createDiagnosticCollection("getaibd.secrets");

const CONFIG_KEY_FIELDS = [
  "api_key",
  "bot_token",
  "api_token",
  "signing_secret",
  "auth_token",
];

export function activateDiagnostics(context: vscode.ExtensionContext): void {
  context.subscriptions.push(COLLECTION);

  if (vscode.window.activeTextEditor) {
    scanDocument(vscode.window.activeTextEditor.document);
  }

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(scanDocument),
    vscode.workspace.onDidSaveTextDocument(scanDocument),
    vscode.workspace.onDidChangeTextDocument((e) => scanDocument(e.document)),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      if (editor) {scanDocument(editor.document);}
    }),
  );
}

function scanDocument(doc: vscode.TextDocument): void {
  const name = doc.fileName.toLowerCase();
  const isConfig =
    name.endsWith("config.toml") ||
    name.endsWith(".env") ||
    name.endsWith(".env.local");
  if (!isConfig) {
    COLLECTION.delete(doc.uri);
    return;
  }

  const text = doc.getText();
  const diagnostics: vscode.Diagnostic[] = [];

  for (const rule of SECRET_RULES) {
    const regex = new RegExp(rule.pattern.source, "g");
    let m: RegExpExecArray | null;
    while ((m = regex.exec(text)) !== null) {
      const start = doc.positionAt(m.index);
      const end = doc.positionAt(m.index + m[0].length);
      const range = new vscode.Range(start, end);
      const diag = new vscode.Diagnostic(
        range,
        `Hardcoded ${rule.label} detected — use an environment variable instead`,
        vscode.DiagnosticSeverity.Warning,
      );
      diag.source = "GetAIBD SafeType";
      diag.code = rule.id;
      diagnostics.push(diag);
    }
  }

  for (const field of CONFIG_KEY_FIELDS) {
    const fieldRegex = new RegExp(
      `${field}\\s*=\\s*"([^"]{8,})"`,
      "g",
    );
    let m: RegExpExecArray | null;
    while ((m = fieldRegex.exec(text)) !== null) {
      const value = m[1];
      const alreadyCaught = diagnostics.some(
        (d) => m!.index < d.range.end.character && m!.index + m![0].length > d.range.start.character,
      );
      if (alreadyCaught) {continue;}
      if (/^[a-zA-Z0-9_-]{20,}$/.test(value)) {
        const valStart = text.indexOf(value, m.index);
        const start = doc.positionAt(valStart);
        const end = doc.positionAt(valStart + value.length);
        const diag = new vscode.Diagnostic(
          new vscode.Range(start, end),
          `Hardcoded value in '${field}' — consider using an environment variable`,
          vscode.DiagnosticSeverity.Information,
        );
        diag.source = "GetAIBD SafeType";
        diagnostics.push(diag);
      }
    }
  }

  COLLECTION.set(doc.uri, diagnostics);
}
