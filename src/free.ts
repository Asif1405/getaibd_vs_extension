import * as vscode from "vscode";
import { randomUUID, createHash } from "crypto";
import { execFileSync } from "child_process";
import * as os from "os";
import * as fs from "fs";
import * as path from "path";
import { getBaseUrl } from "./util/config";

const DEVICE_ID_KEY = "getaibd.deviceId";

/** Shared device-id file, aligned with the editor/CLI so one machine = one id. */
function deviceIdFile(): string {
  return path.join(os.homedir(), ".getaibd", "device_id");
}

/** SHA-256 of a namespaced fingerprint, truncated to 32 hex chars. */
function stableIdFrom(fp: string): string {
  return createHash("sha256").update(`getaibd-device-v1:${fp}`).digest("hex").slice(0, 32);
}

/**
 * Best-effort stable hardware identifier so deleting the id (or reinstalling the
 * extension) regenerates the *same* device id and can't reset the free quota.
 * macOS: IOPlatformUUID; Linux: /etc/machine-id; Windows: registry MachineGuid.
 */
function machineFingerprint(): string | undefined {
  try {
    if (process.platform === "darwin") {
      const out = execFileSync("ioreg", ["-rd1", "-c", "IOPlatformExpertDevice"], {
        encoding: "utf8",
      });
      return out.match(/IOPlatformUUID"?\s*=\s*"?([0-9A-Fa-f-]+)"?/)?.[1];
    }
    if (process.platform === "linux") {
      for (const p of ["/etc/machine-id", "/var/lib/dbus/machine-id"]) {
        try {
          const s = fs.readFileSync(p, "utf8").trim();
          if (s) return s;
        } catch {
          /* try next */
        }
      }
      return undefined;
    }
    if (process.platform === "win32") {
      const out = execFileSync(
        "reg",
        ["query", "HKLM\\SOFTWARE\\Microsoft\\Cryptography", "/v", "MachineGuid"],
        { encoding: "utf8" },
      );
      return out.match(/MachineGuid\s+REG_SZ\s+([0-9A-Fa-f-]+)/)?.[1];
    }
  } catch {
    /* fingerprint unavailable */
  }
  return undefined;
}

export interface FreeStatus {
  token: string;
  days_limit: number;
  days_used: number;
  days_left: number;
  model: string;
}

/**
 * A stable per-machine device id, shared with the editor/CLI via
 * `~/.getaibd/device_id`. Resolution order: (1) the shared file wins (survives
 * extension reinstall + unifies clients); (2) an existing globalState id keeps
 * continuity for already-installed users; (3) otherwise derive from the machine
 * fingerprint so a reinstall regenerates the same id; (4) random as a last
 * resort. The chosen id is written back to both the file and globalState.
 */
export function getDeviceId(context: vscode.ExtensionContext): string {
  const file = deviceIdFile();
  try {
    const existing = fs.readFileSync(file, "utf8").trim();
    if (existing) return existing;
  } catch {
    /* file missing */
  }

  const fp = machineFingerprint();
  const id =
    context.globalState.get<string>(DEVICE_ID_KEY) ||
    (fp ? stableIdFrom(fp) : undefined) ||
    randomUUID().replace(/-/g, "");

  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, id);
  } catch {
    /* best effort */
  }
  void context.globalState.update(DEVICE_ID_KEY, id);
  return id;
}

/** Mints (or refreshes) a free-tier device token from the platform. */
export async function createFreeSession(context: vscode.ExtensionContext): Promise<FreeStatus> {
  const resp = await fetch(`${getBaseUrl()}/free/session`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ device_id: getDeviceId(context) }),
  });
  if (!resp.ok) {
    let detail = `HTTP ${resp.status}`;
    try {
      const body = (await resp.json()) as { detail?: string };
      if (body.detail) detail = body.detail;
    } catch {
      /* non-JSON error body */
    }
    throw new Error(`Could not start free session: ${detail}`);
  }
  return (await resp.json()) as FreeStatus;
}
