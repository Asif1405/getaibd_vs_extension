import type { ChatMessage } from "../client";

/** Max chars injected per file in simple chat (open editor or @mention). */
export const MAX_FILE_CONTEXT_CHARS = 4_000;

/** Client-side history char budget (engine trims further). */
export const HISTORY_CHAR_BUDGET = 24_000;

/** Rough tokens from char count (~4 chars/token). */
export function estimateTokens(chars: number): number {
  return Math.max(0, Math.ceil(chars / 4));
}

export function truncateFileContent(content: string, maxChars = MAX_FILE_CONTEXT_CHARS): string {
  if (content.length <= maxChars) {
    return content;
  }
  return `${content.slice(0, maxChars)}\n… [truncated ${content.length - maxChars} chars]`;
}

export function contextFingerprint(label: string, content: string): string {
  return `${label}\0${content.length}\0${content.slice(0, 256)}`;
}

export interface ContextEstimate {
  chars: number;
  tokensApprox: number;
  fileAttachments: number;
  historyTurns: number;
}

export function estimatePayload(
  messages: ChatMessage[],
  input = "",
): ContextEstimate {
  let chars = input.length;
  let fileAttachments = 0;
  for (const m of messages) {
    chars += m.content.length;
    if (
      m.content.includes("[Currently open file:") ||
      m.content.includes("[File:") ||
      m.content.includes("[Context:")
    ) {
      fileAttachments += 1;
    }
  }
  const historyTurns = messages.filter((m) => !m.content.startsWith("[")).length;
  return {
    chars,
    tokensApprox: estimateTokens(chars),
    fileAttachments,
    historyTurns,
  };
}

export function formatContextEstimate(est: ContextEstimate): string {
  const k =
    est.tokensApprox >= 1000
      ? `${(est.tokensApprox / 1000).toFixed(1)}k`
      : String(est.tokensApprox);
  const files =
    est.fileAttachments > 0 ? ` · ${est.fileAttachments} file${est.fileAttachments === 1 ? "" : "s"}` : "";
  return `≈${k} ctx${files}`;
}
