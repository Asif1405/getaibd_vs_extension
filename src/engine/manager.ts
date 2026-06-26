import * as vscode from "vscode";
import { spawn, execFile, ChildProcess } from "child_process";
import { randomUUID, createHash } from "crypto";
import { promisify } from "util";
import * as path from "path";
import * as fs from "fs";

const execFileAsync = promisify(execFile);
import {
  getApiKey,
  getBaseUrl,
  getConfiguredServerUrl,
  setAuthToken,
  setEngineUrl,
} from "../util/config";

let engineProcess: ChildProcess | undefined;
let output: vscode.OutputChannel | undefined;
let starting: Promise<string> | undefined;

function log(message: string): void {
  output ??= vscode.window.createOutputChannel("GetAIBD Engine");
  output.appendLine(`[${new Date().toISOString()}] ${message}`);
}

function binaryName(): string {
  return process.platform === "win32" ? "getaibd-agent.exe" : "getaibd-agent";
}

/** Resolves the engine binary: user override, bundled binary, then dev builds. */
function resolveEnginePath(context: vscode.ExtensionContext): string | undefined {
  const override = vscode.workspace
    .getConfiguration("getaibd")
    .get<string>("enginePath", "")
    .trim();
  const candidates = [
    override,
    path.join(context.extensionPath, "bin", binaryName()),
    path.join(context.extensionPath, "engine", "target", "release", binaryName()),
    path.join(context.extensionPath, "engine", "target", "debug", binaryName()),
  ];
  return candidates.find((p) => p && fs.existsSync(p));
}

/** Kills any stale process still bound to the engine port (e.g. an orphan from a
 * previous session whose auth token we no longer know). Best-effort, cross-platform. */
async function freePort(port: string): Promise<void> {
  try {
    if (process.platform === "win32") {
      const { stdout } = await execFileAsync("netstat", ["-ano", "-p", "tcp"]);
      const pids = new Set<string>();
      for (const line of stdout.split(/\r?\n/)) {
        if (line.includes(`:${port}`) && /LISTENING/i.test(line)) {
          const pid = line.trim().split(/\s+/).pop();
          if (pid && pid !== "0") {
            pids.add(pid);
          }
        }
      }
      for (const pid of pids) {
        await execFileAsync("taskkill", ["/F", "/PID", pid]).catch(() => undefined);
      }
    } else {
      const { stdout } = await execFileAsync("lsof", [
        "-nP",
        `-iTCP:${port}`,
        "-sTCP:LISTEN",
        "-t",
      ]).catch(() => ({ stdout: "" }));
      for (const pid of stdout.split(/\s+/).filter(Boolean)) {
        try {
          process.kill(Number(pid));
        } catch {
          /* already gone */
        }
      }
    }
  } catch {
    /* nothing listening, or tool unavailable */
  }
  if (await isHealthy(`http://127.0.0.1:${port}`, 1500)) {
    await new Promise((r) => setTimeout(r, 500));
  }
}

async function isHealthy(url: string, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 1000);
      const resp = await fetch(`${url}/health`, { signal: controller.signal });
      clearTimeout(timer);
      if (resp.ok) {
        return true;
      }
    } catch {
      /* engine not up yet */
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

/** Ignore files we extend when the project already uses them (never created). */
const EXTRA_IGNORE_FILES = [
  ".cursorignore",
  ".dockerignore",
  ".vscodeignore",
  ".npmignore",
  ".eslintignore",
  ".prettierignore",
  ".aiexclude",
  ".aiignore",
];

/**
 * Keeps `.getaibd/` out of source control and tooling: creates/updates
 * `.gitignore` in git repos and appends to any other ignore files the project
 * already uses (without creating new ones).
 */
function ensureGetaibdIgnored(projectRoot: string): void {
  const append = (file: string, createIfMissing: boolean): void => {
    try {
      const target = path.join(projectRoot, file);
      const exists = fs.existsSync(target);
      if (!exists && !createIfMissing) {
        return;
      }
      const current = exists ? fs.readFileSync(target, "utf8") : "";
      if (/^\.getaibd\/?\s*$/m.test(current)) {
        return;
      }
      const prefix = current && !current.endsWith("\n") ? "\n" : "";
      fs.appendFileSync(target, `${prefix}.getaibd/\n`);
    } catch {
      /* best effort */
    }
  };

  if (fs.existsSync(path.join(projectRoot, ".git"))) {
    append(".gitignore", true);
  }
  for (const file of EXTRA_IGNORE_FILES) {
    append(file, false);
  }
}

/** Copy root `MEMORY.md` into `.getaibd/MEMORY.md` once when only the legacy file exists. */
function ensureGetaibdMemoryMigrated(projectRoot: string): void {
  try {
    const getaibdDir = path.join(projectRoot, ".getaibd");
    const nested = path.join(getaibdDir, "MEMORY.md");
    const legacy = path.join(projectRoot, "MEMORY.md");
    if (fs.existsSync(nested) || !fs.existsSync(legacy)) {
      return;
    }
    fs.mkdirSync(getaibdDir, { recursive: true });
    fs.copyFileSync(legacy, nested);
    log("Migrated MEMORY.md → .getaibd/MEMORY.md");
  } catch {
    /* best effort */
  }
}

/** Ensures a healthy engine is running and returns its URL. Idempotent. */
export async function ensureEngine(context: vscode.ExtensionContext): Promise<string> {
  if (engineProcess && engineProcess.exitCode === null) {
    return getConfiguredServerUrl();
  }
  starting ??= startEngine(context).finally(() => {
    starting = undefined;
  });
  return starting;
}

async function startEngine(context: vscode.ExtensionContext): Promise<string> {
  const apiKey = await getApiKey(context.secrets);
  if (!apiKey) {
    throw new Error('GetAIBD API key not set. Run "GetAIBD: Set API Key".');
  }

  const serverUrl = getConfiguredServerUrl();
  const port = new URL(serverUrl).port || "39377";

  const enginePath = resolveEnginePath(context);
  if (!enginePath) {
    throw new Error(
      "GetAIBD engine binary not found. Build it with `cargo build --release` in engine/ or set getaibd.enginePath.",
    );
  }

  const token = randomUUID();
  const projectRoot =
    vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? context.extensionPath;
  const memoryEnabled = vscode.workspace
    .getConfiguration("getaibd")
    .get<boolean>("memory", true);
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    GETAIBD_API_KEY: apiKey,
    GETAIBD_BASE_URL: getBaseUrl(),
    MCP_SERVER_HOST: "127.0.0.1",
    MCP_SERVER_PORT: port,
    MCP_AUTH_TOKEN: token,
    MCP_AGENT_PROJECT_ROOT: projectRoot,
    MCP_MEMORY_ENABLED: memoryEnabled ? "1" : "0",
  };
  if (memoryEnabled) {
    const memDir = path.join(context.globalStoragePath, "memory");
    fs.mkdirSync(memDir, { recursive: true });
    // Key the per-project memory DB by a hash of the FULL path. The previous
    // `hex(path).slice(0, 40)` only kept the first 20 characters of the path, so
    // every project under the same parent dir (e.g. ~/Desktop/*) collided into a
    // single shared DB — leaking one project's learned facts into another.
    const key = createHash("sha1").update(projectRoot).digest("hex").slice(0, 16);
    env.MCP_MEMORY_DB_PATH = path.join(memDir, `${key}.db`);
  }
  const model = vscode.workspace
    .getConfiguration("getaibd")
    .get<string>("model", "")
    .trim();
  if (model) {
    env.GETAIBD_DEFAULT_MODEL = model;
  }

  ensureGetaibdIgnored(projectRoot);
  ensureGetaibdMemoryMigrated(projectRoot);

  await freePort(port);

  log(`Starting engine: ${enginePath} (port ${port}, root ${projectRoot})`);
  const child = spawn(enginePath, [], {
    env,
    cwd: projectRoot,
    stdio: ["ignore", "pipe", "pipe"],
  });
  engineProcess = child;
  let exited = false;
  child.once("exit", () => {
    exited = true;
  });

  child.stdout?.on("data", (d: Buffer) => log(d.toString().trimEnd()));
  child.stderr?.on("data", (d: Buffer) => log(d.toString().trimEnd()));
  child.on("exit", (code) => {
    log(`Engine exited with code ${code}`);
    engineProcess = undefined;
    setAuthToken(undefined);
    setEngineUrl(undefined);
  });

  const healthy = await isHealthy(serverUrl, 30000);
  if (exited || child.exitCode !== null) {
    engineProcess = undefined;
    throw new Error("GetAIBD engine exited during startup (port may be in use). See the GetAIBD Engine output for details.");
  }
  if (!healthy) {
    child.kill();
    engineProcess = undefined;
    throw new Error("GetAIBD engine did not become healthy in time. See the GetAIBD Engine output for details.");
  }

  log(`Engine healthy at ${serverUrl}`);
  setAuthToken(token);
  setEngineUrl(serverUrl);
  return serverUrl;
}

/** Stops the spawned engine, if any. */
export function stopEngine(): void {
  if (engineProcess) {
    log("Stopping engine");
    engineProcess.kill();
    engineProcess = undefined;
  }
  setAuthToken(undefined);
  setEngineUrl(undefined);
}

/** Restarts the engine (e.g. after the API key changes). */
export async function restartEngine(context: vscode.ExtensionContext): Promise<string> {
  stopEngine();
  return ensureEngine(context);
}
