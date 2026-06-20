import * as vscode from "vscode";

export type ShellKind = "powershell" | "cmd" | "posix";

/** Best-effort detection of the integrated terminal's shell family. */
export function detectShellKind(): ShellKind {
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

/** Human-readable host OS + shell reported to the agent so it generates
 * commands for the environment they actually run in. */
export function describeEnvironment(): { os: string; shell: string } {
  const os =
    process.platform === "win32"
      ? "Windows"
      : process.platform === "darwin"
        ? "macOS"
        : "Linux";

  const base = (vscode.env.shell || "").split(/[\\/]/).pop() || "";
  const kind = detectShellKind();
  const shell =
    kind === "powershell"
      ? "PowerShell"
      : kind === "cmd"
        ? "Command Prompt (cmd.exe)"
        : base || "POSIX shell (bash/zsh)";

  return { os, shell };
}
