/** Narrowing helpers for untrusted webview → extension-host messages (M-3). */

export interface UntrustedWebviewMessage {
  type: string;
  [key: string]: unknown;
}

export function msgString(msg: UntrustedWebviewMessage, key: string): string | undefined {
  const v = msg[key];
  return typeof v === "string" ? v : undefined;
}

export function msgBool(msg: UntrustedWebviewMessage, key: string): boolean {
  return msg[key] === true;
}

export function msgNumber(msg: UntrustedWebviewMessage, key: string): number | undefined {
  const v = msg[key];
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

export function msgStringArray(msg: UntrustedWebviewMessage, key: string): string[] {
  const v = msg[key];
  if (!Array.isArray(v)) {
    return [];
  }
  return v.filter((x): x is string => typeof x === "string");
}
