import * as vscode from "vscode";
import { spawn, ChildProcess } from "child_process";
import { randomUUID } from "crypto";
import * as path from "path";
import * as fs from "fs";
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
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    GETAIBD_API_KEY: apiKey,
    GETAIBD_BASE_URL: getBaseUrl(),
    MCP_SERVER_HOST: "127.0.0.1",
    MCP_SERVER_PORT: port,
    MCP_AUTH_TOKEN: token,
  };
  const model = vscode.workspace
    .getConfiguration("getaibd")
    .get<string>("model", "")
    .trim();
  if (model) {
    env.GETAIBD_DEFAULT_MODEL = model;
  }

  log(`Starting engine: ${enginePath} (port ${port})`);
  const child = spawn(enginePath, [], {
    env,
    cwd: context.extensionPath,
    stdio: ["ignore", "pipe", "pipe"],
  });
  engineProcess = child;

  child.stdout?.on("data", (d: Buffer) => log(d.toString().trimEnd()));
  child.stderr?.on("data", (d: Buffer) => log(d.toString().trimEnd()));
  child.on("exit", (code) => {
    log(`Engine exited with code ${code}`);
    engineProcess = undefined;
    setAuthToken(undefined);
    setEngineUrl(undefined);
  });

  const healthy = await isHealthy(serverUrl, 30000);
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
