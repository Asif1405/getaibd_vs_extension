import * as vscode from "vscode";
import * as crypto from "crypto";
import { getServerUrl, authHeaders } from "../util/config";

export interface EditPreview {
  file: string;
  operation: string;
  valid: boolean;
  error?: string;
  before_lines?: number;
  after_lines?: number;
}

export interface PatchPreviewData {
  plan: string;
  previews: EditPreview[];
  all_valid: boolean;
  llm_response: string;
}

export class PatchPreviewPanel {
  private static instance: PatchPreviewPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private data: PatchPreviewData;
  private disposables: vscode.Disposable[] = [];

  private constructor(data: PatchPreviewData, context: vscode.ExtensionContext) {
    this.data = data;
    this.panel = vscode.window.createWebviewPanel(
      "mcpPatchPreview",
      "Patch Preview",
      vscode.ViewColumn.Beside,
      { enableScripts: true }
    );
    this.panel.webview.html = this.getHtml();
    this.panel.webview.onDidReceiveMessage(
      (msg) => this.handleMessage(msg, context),
      undefined,
      this.disposables
    );
    this.panel.onDidDispose(() => this.dispose(), undefined, this.disposables);
  }

  static show(data: PatchPreviewData, context: vscode.ExtensionContext) {
    if (PatchPreviewPanel.instance) {
      PatchPreviewPanel.instance.panel.dispose();
    }
    PatchPreviewPanel.instance = new PatchPreviewPanel(data, context);
  }

  private async handleMessage(msg: { type: string }, _context: vscode.ExtensionContext) {
    if (msg.type === "apply") {
      await this.applyPatch();
    } else if (msg.type === "cancel") {
      this.panel.dispose();
    }
  }

  private async applyPatch() {
    const url = `${getServerUrl()}/patch/apply`;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify({ llm_response: this.data.llm_response }),
      });
      const json = await res.json() as { result: { success: boolean; failed: { file: string; error?: string }[] } };
      if (json.result.success) {
        vscode.window.showInformationMessage("Patch applied successfully!");
        this.panel.dispose();
      } else {
        const errors = json.result.failed.map((f: { file: string; error?: string }) => `${f.file}: ${f.error}`).join("\n");
        vscode.window.showErrorMessage(`Patch failed:\n${errors}`);
      }
    } catch (err: unknown) {
      vscode.window.showErrorMessage(`Apply failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  private dispose() {
    PatchPreviewPanel.instance = undefined;
    for (const d of this.disposables) {d.dispose();}
  }

  private getHtml(): string {
    const { plan, previews, all_valid } = this.data;
    const nonce = getNonce();
    const cspSource = this.panel.webview.cspSource;

    const rows = previews.map((p) => {
      const icon = p.valid ? "✅" : "❌";
      const lineInfo = p.before_lines !== undefined
        ? `${p.before_lines} → ${p.after_lines ?? 0} lines`
        : "";
      const errorCell = p.error ? `<span class="error">${escapeHtml(p.error)}</span>` : "";
      return `<tr class="${p.valid ? "" : "invalid-row"}">
        <td>${icon}</td>
        <td><code>${escapeHtml(p.file)}</code></td>
        <td><span class="op op-${p.operation}">${p.operation}</span></td>
        <td>${lineInfo}</td>
        <td>${errorCell}</td>
      </tr>`;
    }).join("");

    const applyDisabled = all_valid ? "" : "disabled";
    const statusText = all_valid
      ? `<span class="valid">All ${previews.length} edits are valid</span>`
      : `<span class="invalid">Some edits have errors — fix before applying</span>`;

    return /*html*/`<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline' ${cspSource}; script-src 'nonce-${nonce}';">
<style>
  body { font-family: var(--vscode-font-family, sans-serif); font-size: 13px; padding: 16px; background: var(--vscode-editor-background); color: var(--vscode-editor-foreground); }
  h2 { margin-bottom: 4px; }
  .plan { background: var(--vscode-textBlockQuote-background); border-left: 3px solid var(--vscode-button-background); padding: 8px 12px; margin-bottom: 16px; border-radius: 0 4px 4px 0; font-style: italic; }
  table { width: 100%; border-collapse: collapse; margin-bottom: 16px; }
  th { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--vscode-panel-border); font-size: 11px; text-transform: uppercase; color: var(--vscode-descriptionForeground); }
  td { padding: 6px 8px; border-bottom: 1px solid var(--vscode-panel-border, #333); }
  .invalid-row { background: var(--vscode-inputValidation-errorBackground, #3d1414); }
  code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
  .op { padding: 2px 6px; border-radius: 3px; font-size: 11px; font-weight: 600; }
  .op-modify { background: #1a3a6b; color: #6fb3f2; }
  .op-create { background: #1a3d1a; color: #6ec46e; }
  .op-delete { background: #3d1414; color: #f26e6e; }
  .op-rename { background: #3d3214; color: #f2c96e; }
  .error { color: var(--vscode-errorForeground, #f44747); font-size: 11px; }
  .valid { color: #6ec46e; font-weight: 600; }
  .invalid { color: #f44747; font-weight: 600; }
  .status { margin-bottom: 12px; }
  .actions { display: flex; gap: 10px; }
  button { padding: 8px 20px; border: none; border-radius: 4px; cursor: pointer; font-size: 13px; font-weight: 500; }
  .apply-btn { background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
  .apply-btn:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
  .apply-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .cancel-btn { background: var(--vscode-input-background); color: var(--vscode-input-foreground); border: 1px solid var(--vscode-input-border); }
</style>
</head>
<body>
  <h2>Patch Preview</h2>
  <div class="plan">${escapeHtml(plan)}</div>
  <div class="status">${statusText}</div>
  <table>
    <thead><tr><th></th><th>File</th><th>Operation</th><th>Lines</th><th>Error</th></tr></thead>
    <tbody>${rows}</tbody>
  </table>
  <div class="actions">
    <button class="apply-btn" id="applyBtn" ${applyDisabled}>Apply ${previews.length} Edit${previews.length !== 1 ? "s" : ""}</button>
    <button class="cancel-btn" id="cancelBtn">Cancel</button>
  </div>
<script nonce="${nonce}">
  const vscode = acquireVsCodeApi();
  const applyBtn = document.getElementById("applyBtn");
  const cancelBtn = document.getElementById("cancelBtn");
  if (applyBtn) { applyBtn.addEventListener("click", () => vscode.postMessage({ type: "apply" })); }
  if (cancelBtn) { cancelBtn.addEventListener("click", () => vscode.postMessage({ type: "cancel" })); }
</script>
</body>
</html>`;
  }
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function getNonce(): string {
  return crypto.randomBytes(16).toString("base64url");
}
