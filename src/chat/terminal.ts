import * as vscode from "vscode";
import { exec, type ChildProcess } from "child_process";
import { detectShellKind, type ShellKind } from "../util/environment";

export interface CommandResult {
  stdout: string;
  stderr: string;
  exit_code: number;
  /** Id of the pooled terminal the command ran in (for follow-ups / read_terminal). */
  terminal_id?: string;
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

// Cap how much scrollback we retain per run so a chatty server can't grow unbounded.
const MAX_RUN_OUTPUT = 100_000;

function bgNote(id: string): string {
  return (
    `\n\n[Still running in the background terminal "${id}". It started successfully and was ` +
    `released so you can continue — do NOT re-run or kill it. Use read_terminal with ` +
    `terminal_id "${id}" to read its latest output.]`
  );
}

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

/** A single command run recorded against a pooled terminal. */
interface RunRecord {
  command: string;
  cwd?: string;
  output: string;
  /** null while still running (released background process); otherwise the exit code. */
  exitCode: number | null;
  startedAt: number;
}

/** A persistent terminal in the pool, with its own run history. */
interface PooledTerminal {
  id: string;
  terminal: vscode.Terminal;
  /** A foreground command is currently executing in it. */
  busy: boolean;
  /** Holds a released long-running process (dev server/watcher) — alive, not reusable. */
  running: boolean;
  runs: RunRecord[];
}

/**
 * Manages a Cursor-like pool of persistent, visible terminals for agent commands:
 * idle terminals are reused, a new one is created only when every terminal is busy, and
 * running processes are never interrupted or reused. Per-terminal scrollback is kept for
 * the whole session so the agent can read earlier runs across all terminals.
 */
export class AgentTerminal {
  private pool: PooledTerminal[] = [];
  private counter = 0;
  /** Id of the terminal whose command is currently executing (cancel target). */
  private activeId: string | undefined;
  private cancelled = false;

  constructor(private readonly root: vscode.Uri | undefined) {}

  /** Whether the running VS Code build exposes terminal shell integration. */
  static get supported(): boolean {
    return typeof vscode.window.onDidStartTerminalShellExecution === "function";
  }

  /** Drops terminals the user manually closed; keeps live ones (incl. running ones). */
  private prune(): void {
    this.pool = this.pool.filter((t) => t.terminal.exitStatus === undefined);
  }

  /** Picks an idle terminal to reuse, or creates a new one if all are busy/running. */
  private pickTerminal(preferredId?: string): PooledTerminal {
    this.prune();
    const idle = (t: PooledTerminal) => !t.busy && !t.running;
    if (preferredId) {
      const wanted = this.pool.find((t) => t.id === preferredId && idle(t));
      if (wanted) {
        return wanted;
      }
    }
    const free = this.pool.find(idle);
    if (free) {
      return free;
    }
    return this.createTerminal();
  }

  private createTerminal(): PooledTerminal {
    this.counter += 1;
    const id = `agent-${this.counter}`;
    const terminal = vscode.window.createTerminal({
      name: `GetAIBD Agent ${this.counter}`,
      cwd: this.root,
      iconPath: new vscode.ThemeIcon("robot"),
    });
    const entry: PooledTerminal = { id, terminal, busy: false, running: false, runs: [] };
    this.pool.push(entry);
    return entry;
  }

  /** Closes idle terminals; leaves running processes untouched (per the "never close
   * a running terminal" contract). Called on panel teardown. */
  dispose(): void {
    for (const t of this.pool) {
      if (!t.running && !t.busy) {
        t.terminal.dispose();
      }
    }
    this.pool = [];
    this.activeId = undefined;
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
    const active = this.pool.find((t) => t.id === this.activeId);
    active?.terminal.sendText("\u0003");
  }

  async run(
    cmd: string,
    args: string[],
    cwd: string | undefined,
    onChunk: (s: string) => void,
    terminalId?: string,
  ): Promise<CommandResult> {
    this.cancelled = false;
    const line = buildLine(cmd, args);
    const targetDir = this.resolveDir(cwd);
    const full = targetDir ? chainCd(targetDir, line, detectShellKind()) : line;

    const mt = this.pickTerminal(terminalId);
    const record: RunRecord = {
      command: line,
      cwd,
      output: "",
      exitCode: null,
      startedAt: Date.now(),
    };
    mt.runs.push(record);
    mt.busy = true;
    this.activeId = mt.id;
    mt.terminal.show(true);

    try {
      return await this.runInTerminal(mt, record, line, full, cwd, onChunk);
    } finally {
      if (this.activeId === mt.id) {
        this.activeId = undefined;
      }
    }
  }

  private append(record: RunRecord, text: string): void {
    record.output += text;
    if (record.output.length > MAX_RUN_OUTPUT) {
      record.output = record.output.slice(record.output.length - MAX_RUN_OUTPUT);
    }
  }

  private async runInTerminal(
    mt: PooledTerminal,
    record: RunRecord,
    line: string,
    full: string,
    cwd: string | undefined,
    onChunk: (s: string) => void,
  ): Promise<CommandResult> {
    const si = await this.waitForShellIntegration(mt.terminal, 6000);
    if (!si) {
      return this.runFallback(mt, record, line, cwd, onChunk);
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
            this.append(record, clean);
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
        const code = this.cancelled ? 130 : exitCode;
        record.exitCode = code;
        mt.busy = false;
        mt.running = false;
        return { stdout: out, stderr: "", exit_code: code, terminal_id: mt.id };
      }
      if (this.cancelled) {
        record.exitCode = 130;
        mt.busy = false;
        mt.running = false;
        return { stdout: out, stderr: "", exit_code: 130, terminal_id: mt.id };
      }
      if (released) {
        // Long-running process: leave it running in this terminal and mark the terminal
        // as occupied so it's never reused or killed. The next command picks a fresh one.
        mt.busy = false;
        mt.running = true;
        return { stdout: out + bgNote(mt.id), stderr: "", exit_code: 0, terminal_id: mt.id };
      }
      record.exitCode = exitCode;
      mt.busy = false;
      mt.running = false;
      return { stdout: out, stderr: "", exit_code: exitCode, terminal_id: mt.id };
    } catch {
      return this.runFallback(mt, record, line, cwd, onChunk);
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
    mt: PooledTerminal,
    record: RunRecord,
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
      let child: ChildProcess;
      const finish = (code: number, released?: boolean) => {
        if (done) {
          return;
        }
        done = true;
        clearInterval(timer);
        record.exitCode = released ? null : code;
        mt.busy = false;
        mt.running = !!released;
        resolve({
          stdout: out + (released ? bgNote(mt.id) : ""),
          stderr: errOut,
          exit_code: released ? 0 : code,
          terminal_id: mt.id,
        });
      };
      child = exec(line, { cwd: cwdAbs, shell, maxBuffer: 16 * 1024 * 1024 });
      child.stdout?.on("data", (d) => {
        const s = String(d);
        out += s;
        this.append(record, s);
        lastAt = Date.now();
        onChunk(s);
      });
      child.stderr?.on("data", (d) => {
        const s = String(d);
        errOut += s;
        this.append(record, s);
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
          finish(0, true);
        }
      }, 500);
    });
  }

  /** Renders earlier terminal output for the agent. With an id, returns that terminal's
   * full scrollback; without, lists every terminal and the tail of its recent runs. */
  readHistory(terminalId?: string): CommandResult {
    this.prune();
    if (this.pool.length === 0) {
      return {
        stdout: "No terminals have run commands in this session yet.",
        stderr: "",
        exit_code: 0,
      };
    }
    if (terminalId) {
      const mt = this.pool.find((t) => t.id === terminalId);
      if (!mt) {
        const ids = this.pool.map((t) => t.id).join(", ");
        return {
          stdout: `No terminal "${terminalId}" in this session. Active terminals: ${ids}.`,
          stderr: "",
          exit_code: 1,
        };
      }
      return {
        stdout: this.formatTerminal(mt, 12_000),
        stderr: "",
        exit_code: 0,
        terminal_id: mt.id,
      };
    }
    const blocks = this.pool.map((t) => this.formatTerminal(t, 2_000));
    return { stdout: blocks.join("\n\n"), stderr: "", exit_code: 0 };
  }

  private formatTerminal(mt: PooledTerminal, perRunCap: number): string {
    const status = mt.busy ? "busy" : mt.running ? "running (background process)" : "idle";
    const header = `Terminal "${mt.id}" [${status}] — ${mt.runs.length} run(s)`;
    if (mt.runs.length === 0) {
      return header;
    }
    const runs = mt.runs.map((r, i) => {
      const state =
        r.exitCode === null ? "still running" : `exit ${r.exitCode}`;
      let output = r.output.trimEnd();
      if (output.length > perRunCap) {
        output = "…(truncated)…\n" + output.slice(output.length - perRunCap);
      }
      const body = output ? `\n${output}` : "\n(no output)";
      return `  [${i + 1}] $ ${r.command}  (${state})${body.replace(/\n/g, "\n  ")}`;
    });
    return `${header}\n${runs.join("\n")}`;
  }
}
