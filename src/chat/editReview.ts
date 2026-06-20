import * as vscode from "vscode";

interface PendingEdit {
  relPath: string;
  originalOld: string;
  latestNew: string;
}

interface Hunk {
  newStart: number;
  newCount: number;
  oldStart: number;
  oldCount: number;
}

function splitLines(text: string): string[] {
  return text.length ? text.split("\n") : [];
}

/** Structured hunks (old + new line coordinates) between two texts, via LCS. */
function computeHunks(oldText: string, newText: string): Hunk[] {
  const a = splitLines(oldText);
  const b = splitLines(newText);
  const n = a.length;
  const m = b.length;
  if (n * m > 4_000_000) {return [];}
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const ops: Array<{ t: " " | "-" | "+"; oi: number; ni: number }> = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { ops.push({ t: " ", oi: i, ni: j }); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) { ops.push({ t: "-", oi: i, ni: j }); i++; }
    else { ops.push({ t: "+", oi: i, ni: j }); j++; }
  }
  while (i < n) { ops.push({ t: "-", oi: i, ni: j }); i++; }
  while (j < m) { ops.push({ t: "+", oi: i, ni: j }); j++; }

  const hunks: Hunk[] = [];
  let k = 0;
  while (k < ops.length) {
    if (ops[k].t === " ") { k++; continue; }
    const start = k;
    while (k < ops.length && ops[k].t !== " ") { k++; }
    const seg = ops.slice(start, k);
    const plus = seg.filter((o) => o.t === "+");
    const minus = seg.filter((o) => o.t === "-");
    hunks.push({
      newStart: plus.length ? plus[0].ni : seg[0].ni,
      newCount: plus.length,
      oldStart: minus.length ? minus[0].oi : seg[0].oi,
      oldCount: minus.length,
    });
  }
  return hunks;
}

/** Whole-line ranges (in the new content) that were added or changed. */
function addedLineRanges(oldText: string, newText: string): vscode.Range[] {
  const ranges: vscode.Range[] = [];
  for (const h of computeHunks(oldText, newText)) {
    for (let l = 0; l < h.newCount; l++) {
      const line = h.newStart + l;
      ranges.push(new vscode.Range(line, 0, line, 0));
    }
  }
  return ranges;
}

/** Tracks agent edits and surfaces them inline with whole-file and per-block Accept/Reject. */
export class EditReviewManager implements vscode.CodeLensProvider {
  private edits = new Map<string, PendingEdit>();
  private readonly added: vscode.TextEditorDecorationType;
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;
  private onResolved?: (relPath: string, action: "accept" | "reject") => void;
  private onEditChanged?: (relPath: string, originalOld: string, latestNew: string) => void;

  constructor(private readonly root: vscode.Uri | undefined) {
    this.added = vscode.window.createTextEditorDecorationType({
      backgroundColor: new vscode.ThemeColor("diffEditor.insertedTextBackground"),
      isWholeLine: true,
      overviewRulerColor: new vscode.ThemeColor("editorOverviewRuler.addedForeground"),
      overviewRulerLane: vscode.OverviewRulerLane.Left,
    });
  }

  setOnResolved(cb: (relPath: string, action: "accept" | "reject") => void): void {
    this.onResolved = cb;
  }

  setOnEditChanged(cb: (relPath: string, originalOld: string, latestNew: string) => void): void {
    this.onEditChanged = cb;
  }

  register(): vscode.Disposable[] {
    return [
      this.added,
      this.changed,
      vscode.languages.registerCodeLensProvider({ scheme: "file" }, this),
      vscode.commands.registerCommand("getaibd.acceptFileEdit", (p: string) => this.accept(p)),
      vscode.commands.registerCommand("getaibd.rejectFileEdit", (p: string) => this.reject(p)),
      vscode.commands.registerCommand("getaibd.acceptHunk", (p: string, idx: number) => this.acceptHunk(p, idx)),
      vscode.commands.registerCommand("getaibd.rejectHunk", (p: string, idx: number) => this.rejectHunk(p, idx)),
      vscode.window.onDidChangeActiveTextEditor((e) => this.decorate(e)),
      vscode.workspace.onDidOpenTextDocument(() => this.refresh()),
    ];
  }

  private absFor(relPath: string): string | undefined {
    return this.root ? vscode.Uri.joinPath(this.root, relPath).fsPath : undefined;
  }

  /** Records a fresh agent edit and lights it up in the editor if open. */
  addEdit(relPath: string, originalOld: string, latestNew: string): void {
    const abs = this.absFor(relPath);
    if (!abs) {return;}
    this.edits.set(abs, { relPath, originalOld, latestNew });
    this.changed.fire();
    this.refresh();
  }

  /** Keeps the file as written and clears the review state. */
  accept(relPath: string): void {
    const abs = this.absFor(relPath);
    if (abs) {this.edits.delete(abs);}
    this.changed.fire();
    this.refresh();
    this.onResolved?.(relPath, "accept");
  }

  /** Restores the original content (or deletes a newly created file) and clears review. */
  async reject(relPath: string): Promise<void> {
    const abs = this.absFor(relPath);
    const edit = abs ? this.edits.get(abs) : undefined;
    if (abs) {this.edits.delete(abs);}
    if (edit && this.root) {
      const uri = vscode.Uri.joinPath(this.root, edit.relPath);
      try {
        if (edit.originalOld.length === 0) {
          await vscode.workspace.fs.delete(uri, { useTrash: true });
        } else {
          await vscode.workspace.fs.writeFile(uri, Buffer.from(edit.originalOld, "utf8"));
        }
      } catch {
        /* file may have been moved or deleted */
      }
    }
    this.changed.fire();
    this.refresh();
    this.onResolved?.(relPath, "reject");
  }

  /** Accepts a single diff block: folds it into the baseline so it no longer shows. */
  private acceptHunk(relPath: string, index: number): void {
    const abs = this.absFor(relPath);
    const edit = abs ? this.edits.get(abs) : undefined;
    if (!abs || !edit) {return;}
    const hunks = computeHunks(edit.originalOld, edit.latestNew);
    const h = hunks[index];
    if (!h) {return;}
    const oldLines = splitLines(edit.originalOld);
    const newLines = splitLines(edit.latestNew);
    const replacement = newLines.slice(h.newStart, h.newStart + h.newCount);
    oldLines.splice(h.oldStart, h.oldCount, ...replacement);
    edit.originalOld = oldLines.join("\n");
    void this.afterHunkOp(relPath, abs, edit);
  }

  /** Rejects a single diff block: reverts just those lines on disk. */
  private async rejectHunk(relPath: string, index: number): Promise<void> {
    const abs = this.absFor(relPath);
    const edit = abs ? this.edits.get(abs) : undefined;
    if (!abs || !edit || !this.root) {return;}
    const hunks = computeHunks(edit.originalOld, edit.latestNew);
    const h = hunks[index];
    if (!h) {return;}
    const oldLines = splitLines(edit.originalOld);
    const newLines = splitLines(edit.latestNew);
    const replacement = oldLines.slice(h.oldStart, h.oldStart + h.oldCount);
    newLines.splice(h.newStart, h.newCount, ...replacement);
    edit.latestNew = newLines.join("\n");
    try {
      const uri = vscode.Uri.joinPath(this.root, relPath);
      await vscode.workspace.fs.writeFile(uri, Buffer.from(edit.latestNew, "utf8"));
    } catch {
      /* file may have been moved */
    }
    void this.afterHunkOp(relPath, abs, edit);
  }

  /** Common cleanup after a per-block op: resolve if no diff remains, else refresh. */
  private afterHunkOp(relPath: string, abs: string, edit: PendingEdit): void {
    this.onEditChanged?.(relPath, edit.originalOld, edit.latestNew);
    if (computeHunks(edit.originalOld, edit.latestNew).length === 0) {
      this.edits.delete(abs);
      this.onResolved?.(relPath, "accept");
    }
    this.changed.fire();
    this.refresh();
  }

  /** Drops review state for one file without touching disk (used after a checkpoint restore). */
  dropEdit(relPath: string): void {
    const abs = this.absFor(relPath);
    if (abs) {this.edits.delete(abs);}
    this.changed.fire();
    this.refresh();
  }

  clearAll(): void {
    this.edits.clear();
    this.changed.fire();
    this.refresh();
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const edit = this.edits.get(document.uri.fsPath);
    if (!edit) {return [];}
    const hunks = computeHunks(edit.originalOld, edit.latestNew);
    const lenses: vscode.CodeLens[] = [];
    const top = new vscode.Range(0, 0, 0, 0);
    const n = hunks.length;
    lenses.push(
      new vscode.CodeLens(top, { title: `$(check-all) Accept all (${n})`, command: "getaibd.acceptFileEdit", arguments: [edit.relPath] }),
      new vscode.CodeLens(top, { title: "$(discard) Reject all", command: "getaibd.rejectFileEdit", arguments: [edit.relPath] }),
    );
    hunks.forEach((h, idx) => {
      const line = Math.max(0, Math.min(h.newStart, document.lineCount - 1));
      const range = new vscode.Range(line, 0, line, 0);
      lenses.push(
        new vscode.CodeLens(range, { title: "$(check) Accept block", command: "getaibd.acceptHunk", arguments: [edit.relPath, idx] }),
        new vscode.CodeLens(range, { title: "$(discard) Reject block", command: "getaibd.rejectHunk", arguments: [edit.relPath, idx] }),
      );
    });
    return lenses;
  }

  private refresh(): void {
    this.decorate(vscode.window.activeTextEditor);
  }

  private decorate(editor: vscode.TextEditor | undefined): void {
    if (!editor) {return;}
    const edit = this.edits.get(editor.document.uri.fsPath);
    if (!edit) {
      editor.setDecorations(this.added, []);
      return;
    }
    editor.setDecorations(this.added, addedLineRanges(edit.originalOld, edit.latestNew));
  }
}
