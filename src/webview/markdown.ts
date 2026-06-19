import { marked } from "marked";

marked.setOptions({ breaks: true, gfm: true });

/** Renders GitHub-flavoured markdown to HTML for the chat webview. */
function renderMarkdown(src: string): string {
  try {
    return marked.parse(src ?? "", { async: false }) as string;
  } catch {
    return (src ?? "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }
}

(globalThis as unknown as { renderMarkdown: typeof renderMarkdown }).renderMarkdown =
  renderMarkdown;
