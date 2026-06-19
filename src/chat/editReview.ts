import * as vscode from "vscode";

interface PendingEdit {
  relPath: string;
  originalOld: string;
  latestNew: string;
}

/** Lines (0-based, in the new content) that are added or changed vs the old content. */
function addedLineRanges(oldText: string, newText: string): vscode.Range[] {
  const a = oldText.length ? oldText.split("\n") : [];
  const b = newText.split("\n");
  const n = a.length;
  const m = b.length;
  const lcs: number[][] = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const ranges: vscode.Range[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      i++;
    } else {
      ranges.push(new vscode.Range(j, 0, j, b[j].length));
      j++;
    }
  }
  while (j < m) {
    ranges.push(new vscode.Range(j, 0, j, b[j].length));
    j++;
  }
  return ranges;
}

/** Tracks agent edits and surfaces them inline in the editor with Accept/Reject CodeLens. */
export class EditReviewManager implements vscode.CodeLensProvider {
  private edits = new Map<string, PendingEdit>();
  private readonly added: vscode.TextEditorDecorationType;
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;
  private onResolved?: (relPath: string, action: "accept" | "reject") => void;

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

  register(): vscode.Disposable[] {
    return [
      this.added,
      this.changed,
      vscode.languages.registerCodeLensProvider({ scheme: "file" }, this),
      vscode.commands.registerCommand("getaibd.acceptFileEdit", (p: string) => this.accept(p)),
      vscode.commands.registerCommand("getaibd.rejectFileEdit", (p: string) => this.reject(p)),
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

  clearAll(): void {
    this.edits.clear();
    this.changed.fire();
    this.refresh();
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const edit = this.edits.get(document.uri.fsPath);
    if (!edit) {return [];}
    const top = new vscode.Range(0, 0, 0, 0);
    return [
      new vscode.CodeLens(top, {
        title: "$(check) Accept",
        command: "getaibd.acceptFileEdit",
        arguments: [edit.relPath],
      }),
      new vscode.CodeLens(top, {
        title: "$(discard) Reject",
        command: "getaibd.rejectFileEdit",
        arguments: [edit.relPath],
      }),
      new vscode.CodeLens(top, {
        title: "GetAIBD agent edit",
        command: "getaibd.acceptFileEdit",
        arguments: [edit.relPath],
      }),
    ];
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
