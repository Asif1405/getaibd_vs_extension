import DOMPurify from "dompurify";
import { marked } from "marked";

marked.setOptions({ breaks: true, gfm: true });

/** Tags/attrs allowed in assistant markdown. Blocks raw `<script>`, event handlers,
 * and remote `<img>`/`media` loads that could exfiltrate workspace content (C-4). */
const SANITIZE: DOMPurify.Config = {
  ALLOWED_TAGS: [
    "p",
    "br",
    "strong",
    "em",
    "b",
    "i",
    "u",
    "s",
    "del",
    "code",
    "pre",
    "blockquote",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "a",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "hr",
    "span",
    "div",
    "sup",
    "sub",
  ],
  ALLOWED_ATTR: ["href", "title", "class", "id", "target", "rel"],
  ALLOW_DATA_ATTR: false,
};

/** Renders GitHub-flavoured markdown to HTML for the chat webview. */
function renderMarkdown(src: string): string {
  try {
    const raw = marked.parse(src ?? "", { async: false }) as string;
    return DOMPurify.sanitize(raw, SANITIZE);
  } catch {
    return (src ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }
}

(globalThis as unknown as { renderMarkdown: typeof renderMarkdown }).renderMarkdown =
  renderMarkdown;
