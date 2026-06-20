import * as vscode from "vscode";
import { exec } from "child_process";

export interface CommandResult {
  stdout: string;
  stderr: string;
  exit_code: number;
}

const ANSI = /[\u001b\u009b][[\]()#;?]*(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-ORZcf-nqry=><]/g;
const OSC = /[\u001b\u009d]\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g;

// A command that keeps a process in the foreground forever (dev servers, watchers,
// log tails) would otherwise block the agent until the gate's 30-minute timeout.
// We detect these heuristically and release the agent early, leaving the process
// running in its own terminal.
const SERVER_HINT =
  /\b(runserver|uvicorn|gunicorn|hypercorn|daphne|flask\s+run|npm\s+(run\s+)?(dev|start|serve)|yarn\s+(dev|start|serve)|pnpm\s+(dev|start|serve)|bun\s+(dev|run)|vite|next\s+(dev|start)|nuxt\s+dev|nodemon|webpack(\s+serve|-dev-server)|rails\s+s(erver)?|php\s+artisan\s+serve|http\.server|http-server|serve|watch|tail\s+-f|docker\s+compose\s+up(?!\s+-d)|docker\s+logs\s+-f)\b/i;

// Release thresholds: servers get a short leash (they print a banner then idle, or
// stream logs forever); other commands get a generous one so real builds finish.
const SERVER_IDLE_MS = 4_000;
const SERVER_MAX_MS = 12_000;
const DEFAULT_IDLE_MS = 45_000;
const DEFAULT_MAX_MS = 600_000;

const BG_NOTE =
  '\n\n[The command is still running in the background "GetAIBD Agent" terminal. ' +
  "It started successfully and was released so you can continue — do NOT re-run it. " +
  "The process was not stopped.]";

function stripAnsi(text: string): string {
  return text.replace(OSC, "").replace(ANSI, "");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
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

type ShellKind = "powershell" | "cmd" | "posix";

/** Best-effort detection of the integrated terminal's shell family. */
function detectShell(): ShellKind {
  const s = (vscode.env.shell || "").toLowerCase();
  if (s.includes("powershell") || s.includes("pwsh")) {
    return "powershell";
  }
  if (s.includes("cmd.exe") || /(^|[\\/])cmd$/.test(s)) {
    return "cmd";
  }
  if (s) {
    return "posix";
  }
  return process.platform === "win32" ? "powershell" : "posix";
}

/** Prefixes a command with a `cd` using a separator the target shell accepts. */
function chainCd(dir: string, line: string, shell: ShellKind): string {
  const d = buildLine(dir, []);
  if (shell === "powershell") {
    // PowerShell rejects `&&`; `;` chains, and `if ($?)` skips the command if cd fails.
    return `cd ${d}; if ($?) { ${line} }`;
  }
  if (shell === "cmd") {
    // `/d` lets cd switch drives (e.g. d:) as well as directories.
    return `cd /d ${d} && ${line}`;
  }
  return `cd ${d} && ${line}`;
}

/** Runs agent shell commands in a persistent, visible terminal with streamed output. */
export class AgentTerminal {
  private terminal: vscode.Terminal | undefined;
  // Long-running commands (dev servers) that were released keep running here so a
  // reused terminal never collides with a process holding the foreground.
  private backgrounded: vscode.Terminal[] = [];
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
    for (const t of this.backgrounded) {
      t.dispose();
    }
    this.backgrounded = [];
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
    const full = targetDir ? chainCd(targetDir, line, detectShell()) : line;
    const term = this.ensureTerminal();
    term.show(true);

    const si = await this.waitForShellIntegration(term, 6000);
    if (!si) {
      return this.runFallback(line, cwd, onChunk);
    }
    try {
      const execution = si.executeCommand(full);
      let out = "";
      let lastAt = Date.now();
      let exited = false;
      let exitCode = 0;

      this.awaitExit(execution)
        .then((code) => {
          exited = true;
          exitCode = code;
        })
        .catch(() => {
          exited = true;
        });

      void (async () => {
        try {
          for await (const chunk of execution.read()) {
            const clean = stripAnsi(chunk);
            out += clean;
            lastAt = Date.now();
            if (!this.cancelled) {
              onChunk(clean);
            }
          }
        } catch {
          /* stream closed when the terminal is reused/disposed */
        }
      })();

      const isServer = SERVER_HINT.test(line);
      const idleMs = isServer ? SERVER_IDLE_MS : DEFAULT_IDLE_MS;
      const maxMs = isServer ? SERVER_MAX_MS : DEFAULT_MAX_MS;
      const start = Date.now();
      let released = false;
      while (!exited && !this.cancelled) {
        await sleep(400);
        const now = Date.now();
        if (out.length > 0 && now - lastAt >= idleMs) {
          released = true;
          break;
        }
        if (now - start >= maxMs) {
          released = true;
          break;
        }
      }

      if (exited) {
        return { stdout: out, stderr: "", exit_code: this.cancelled ? 130 : exitCode };
      }
      if (this.cancelled) {
        return { stdout: out, stderr: "", exit_code: 130 };
      }
      if (released) {
        // Keep the long-running process alive in its own terminal and start the next
        // command in a fresh one so it doesn't get typed into the running process.
        this.backgrounded.push(term);
        this.terminal = undefined;
        return { stdout: out + BG_NOTE, stderr: "", exit_code: 0 };
      }
      return { stdout: out, stderr: "", exit_code: exitCode };
    } catch {
      return this.runFallback(line, cwd, onChunk);
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

  /** Headless capture when shell integration is unavailable; output is not visible live.
   * Long-running commands are detached (left running) and released instead of hanging. */
  private runFallback(
    line: string,
    cwd: string | undefined,
    onChunk: (s: string) => void,
  ): Promise<CommandResult> {
    const cwdAbs =
      cwd && this.root ? vscode.Uri.joinPath(this.root, cwd).fsPath : this.root?.fsPath;
    const isServer = SERVER_HINT.test(line);
    const idleMs = isServer ? SERVER_IDLE_MS : DEFAULT_IDLE_MS;
    const maxMs = isServer ? SERVER_MAX_MS : DEFAULT_MAX_MS;
    // `cwd` is set via exec options, so the bare command runs as-is. Use PowerShell on
    // Windows to match the integrated terminal (and the engine's command guidance).
    const shell = process.platform === "win32" ? "powershell.exe" : undefined;
    return new Promise((resolve) => {
      let out = "";
      let errOut = "";
      let lastAt = Date.now();
      let done = false;
      const child = exec(line, { cwd: cwdAbs, shell, maxBuffer: 16 * 1024 * 1024 });
      const finish = (code: number, note?: string) => {
        if (done) {
          return;
        }
        done = true;
        clearInterval(timer);
        resolve({ stdout: out + (note ?? ""), stderr: errOut, exit_code: code });
      };
      child.stdout?.on("data", (d) => {
        const s = String(d);
        out += s;
        lastAt = Date.now();
        onChunk(s);
      });
      child.stderr?.on("data", (d) => {
        const s = String(d);
        errOut += s;
        lastAt = Date.now();
        onChunk(s);
      });
      child.on("close", (code) => finish(typeof code === "number" ? code : 0));
      child.on("error", () => finish(1));
      const start = Date.now();
      const timer = setInterval(() => {
        if (this.cancelled) {
          try {
            child.kill("SIGINT");
          } catch {
            /* already gone */
          }
          finish(130);
          return;
        }
        const now = Date.now();
        const idle = out.length + errOut.length > 0 && now - lastAt >= idleMs;
        if (idle || now - start >= maxMs) {
          child.unref();
          finish(0, BG_NOTE);
        }
      }, 500);
    });
  }
}
