import * as vscode from "vscode";
import { exec } from "child_process";

export interface CommandResult {
  stdout: string;
  stderr: string;
  exit_code: number;
}

const ANSI = /[\u001b\u009b][[\]()#;?]*(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-ORZcf-nqry=><]/g;
const OSC = /[\u001b\u009d]\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g;

function stripAnsi(text: string): string {
  return text.replace(OSC, "").replace(ANSI, "");
}

/** Builds a single shell command line from a program plus optional arguments. */
function buildLine(cmd: string, args: string[]): string {
  if (args.length === 0) {
    return cmd;
  }
  const quote = (a: string) =>
    /[^A-Za-z0-9_/:=@%.,+-]/.test(a) ? "'" + a.replace(/'/g, "'\\''") + "'" : a;
  return [cmd, ...args].map(quote).join(" ");
}

/** Runs agent shell commands in a persistent, visible terminal with streamed output. */
export class AgentTerminal {
  private terminal: vscode.Terminal | undefined;
  private cancelled = false;

  constructor(private readonly root: vscode.Uri | undefined) {}

  /** Whether the running VS Code build exposes terminal shell integration. */
  static get supported(): boolean {
    return typeof vscode.window.onDidStartTerminalShellExecution === "function";
  }

  private ensureTerminal(): vscode.Terminal {
    if (this.terminal && this.terminal.exitStatus === undefined) {
      return this.terminal;
    }
    this.terminal = vscode.window.createTerminal({
      name: "GetAIBD Agent",
      cwd: this.root,
      iconPath: new vscode.ThemeIcon("robot"),
    });
    return this.terminal;
  }

  dispose(): void {
    this.terminal?.dispose();
    this.terminal = undefined;
  }

  /** Resolves an absolute working directory so each command runs deterministically. */
  private resolveDir(cwd: string | undefined): string | undefined {
    if (!this.root) {
      return cwd;
    }
    return cwd ? vscode.Uri.joinPath(this.root, cwd).fsPath : this.root.fsPath;
  }

  /** Signals cancellation of the currently running command. */
  cancel(): void {
    this.cancelled = true;
    this.terminal?.sendText("\u0003");
  }

  async run(
    cmd: string,
    args: string[],
    cwd: string | undefined,
    onChunk: (s: string) => void,
  ): Promise<CommandResult> {
    this.cancelled = false;
    const line = buildLine(cmd, args);
    const targetDir = this.resolveDir(cwd);
    const full = targetDir ? `cd ${buildLine(targetDir, [])} && ${line}` : line;
    const term = this.ensureTerminal();
    term.show(true);

    const si = await this.waitForShellIntegration(term, 6000);
    if (!si) {
      return this.runFallback(full, cwd, onChunk);
    }
    try {
      const execution = si.executeCommand(full);
      let out = "";
      for await (const chunk of execution.read()) {
        if (this.cancelled) {
          break;
        }
        const clean = stripAnsi(chunk);
        out += clean;
        onChunk(clean);
      }
      const code = await this.awaitExit(execution);
      return { stdout: out, stderr: "", exit_code: this.cancelled ? 130 : code };
    } catch {
      return this.runFallback(full, cwd, onChunk);
    }
  }

  private waitForShellIntegration(
    term: vscode.Terminal,
    timeoutMs: number,
  ): Promise<vscode.TerminalShellIntegration | undefined> {
    if (term.shellIntegration) {
      return Promise.resolve(term.shellIntegration);
    }
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        sub.dispose();
        resolve(term.shellIntegration);
      }, timeoutMs);
      const sub = vscode.window.onDidChangeTerminalShellIntegration((e) => {
        if (e.terminal === term && e.shellIntegration) {
          clearTimeout(timer);
          sub.dispose();
          resolve(e.shellIntegration);
        }
      });
    });
  }

  private awaitExit(execution: vscode.TerminalShellExecution): Promise<number> {
    return new Promise((resolve) => {
      const sub = vscode.window.onDidEndTerminalShellExecution((e) => {
        if (e.execution === execution) {
          sub.dispose();
          resolve(e.exitCode ?? 0);
        }
      });
    });
  }

  /** Headless capture when shell integration is unavailable; output is not visible live. */
  private runFallback(
    full: string,
    cwd: string | undefined,
    onChunk: (s: string) => void,
  ): Promise<CommandResult> {
    const cwdAbs =
      cwd && this.root ? vscode.Uri.joinPath(this.root, cwd).fsPath : this.root?.fsPath;
    return new Promise((resolve) => {
      exec(full, { cwd: cwdAbs, maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
        const out = String(stdout ?? "");
        const errOut = String(stderr ?? "");
        if (out) {
          onChunk(out);
        }
        if (errOut) {
          onChunk(errOut);
        }
        const code =
          err && typeof (err as { code?: unknown }).code === "number"
            ? ((err as { code: number }).code)
            : err
              ? 1
              : 0;
        resolve({ stdout: out, stderr: errOut, exit_code: code });
      });
    });
  }
}
