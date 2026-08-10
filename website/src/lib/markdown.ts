import { marked } from "marked";
import { tokenize, type TokenKind } from "./helix";

const CLASS: Record<TokenKind, string> = {
  doc: "text-emerald-300/70 italic",
  comment: "text-zinc-500",
  string: "text-amber-300",
  interp: "text-amber-200 font-medium",
  number: "text-sky-300",
  keyword: "text-fuchsia-400",
  builtin: "text-cyan-300",
  method: "text-emerald-300",
  operator: "text-zinc-400",
  punct: "text-zinc-500",
  ident: "text-zinc-200",
  plain: "text-zinc-300",
};

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

/**
 * Render repo markdown to HTML, highlighting `helix` fences with the same tokenizer the
 * rest of the site uses. Everything else is left alone — these documents are engineering
 * records, and rewriting them for presentation is how a doc site starts lying.
 */
export function renderMarkdown(md: string): string {
  const renderer = new marked.Renderer();

  renderer.code = ({ text, lang }: { text: string; lang?: string }) => {
    const language = (lang ?? "").trim().toLowerCase();
    if (language === "helix" || language === "") {
      const inner = tokenize(text)
        .map((t) => `<span class="${CLASS[t.kind]}">${escapeHtml(t.text)}</span>`)
        .join("");
      return `<pre class="overflow-x-auto rounded-lg border border-zinc-800 bg-zinc-900/70 p-4 font-mono text-[13px] leading-6"><code>${inner}</code></pre>`;
    }
    return `<pre class="overflow-x-auto rounded-lg border border-zinc-800 bg-zinc-900/70 p-4 font-mono text-[13px] leading-6"><code>${escapeHtml(
      text
    )}</code></pre>`;
  };

  return marked.parse(md, { renderer, async: false }) as string;
}
