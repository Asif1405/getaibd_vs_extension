import * as vscode from "vscode";

const DEFAULT_SERVER_URL = "http://127.0.0.1:39377";
const DEFAULT_BASE_URL = "https://getaibd.com/v1/api";

/** The free anonymous device token prefix and the single free ("Auto") model. */
export const FREE_TOKEN_PREFIX = "aiobf_";
export const FREE_MODEL_ID = "qwen-flash";
export const FREE_MODEL_LABEL = "Auto";

/** Paid usage stops at or below this balance (matches platform `API_CREDIT_FLOOR`). */
export const CREDIT_FLOOR = 10;
export const BILLING_URL = "https://getaibd.com/dashboard";
export const INTEGRATION_URL = "https://getaibd.com/connections";

/** Whether a stored credential is an anonymous free-tier device token. */
export function isFreeToken(key: string | undefined | null): boolean {
  return !!key && key.startsWith(FREE_TOKEN_PREFIX);
}

let activeEngineUrl: string | undefined;
let authToken: string | undefined;

/** Records the URL of the engine the extension spawned (or reused). */
export function setEngineUrl(url: string | undefined): void {
  activeEngineUrl = url;
}

/** Records the per-spawn bearer token the engine requires on every request. */
export function setAuthToken(token: string | undefined): void {
  authToken = token;
}

/** Authorization header for engine requests, empty when no engine is running. */
export function authHeaders(): Record<string, string> {
  return authToken ? { Authorization: `Bearer ${authToken}` } : {};
}

/** The configured engine bind URL, independent of whether it is running yet. */
export function getConfiguredServerUrl(): string {
  return vscode.workspace
    .getConfiguration("getaibd")
    .get<string>("serverUrl", DEFAULT_SERVER_URL)
    .trim();
}

/** Base URL the extension talks to: the live engine, else the configured fallback. */
export function getServerUrl(): string {
  return activeEngineUrl ?? getConfiguredServerUrl();
}

const API_KEY_SECRET = "getaibd.apiKey";

/** The GetAIBD API key from SecretStorage, migrating any legacy plaintext setting. */
export async function getApiKey(secrets: vscode.SecretStorage): Promise<string> {
  const stored = (await secrets.get(API_KEY_SECRET))?.trim();
  if (stored) {
    return stored;
  }
  const legacy = vscode.workspace.getConfiguration("getaibd").get<string>("apiKey", "").trim();
  if (legacy) {
    await secrets.store(API_KEY_SECRET, legacy);
    await vscode.workspace
      .getConfiguration("getaibd")
      .update("apiKey", undefined, vscode.ConfigurationTarget.Global)
      .then(undefined, () => undefined);
  }
  return legacy;
}

/** Stores (or clears) the GetAIBD API key in SecretStorage. */
export async function setApiKey(secrets: vscode.SecretStorage, key: string): Promise<void> {
  const trimmed = key.trim();
  if (trimmed) {
    await secrets.store(API_KEY_SECRET, trimmed);
  } else {
    await secrets.delete(API_KEY_SECRET);
  }
}

/** GetAIBD OpenAI-compatible base URL the engine routes inference through. */
export function getBaseUrl(): string {
  return vscode.workspace.getConfiguration("getaibd").get<string>("baseUrl", DEFAULT_BASE_URL).trim();
}
