import * as vscode from "vscode";
import { randomUUID } from "crypto";
import { getBaseUrl } from "./util/config";

const DEVICE_ID_KEY = "getaibd.deviceId";

export interface FreeStatus {
  token: string;
  days_limit: number;
  days_used: number;
  days_left: number;
  model: string;
}

/** A stable per-install device id, generated once and persisted. */
export function getDeviceId(context: vscode.ExtensionContext): string {
  let id = context.globalState.get<string>(DEVICE_ID_KEY);
  if (!id) {
    id = randomUUID().replace(/-/g, "");
    void context.globalState.update(DEVICE_ID_KEY, id);
  }
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
