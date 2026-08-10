"use client";

import { useState } from "react";

/**
 * A copy button for command blocks. Copies the COMMANDS ONLY — never the `$` prompt and
 * never the expected output, because the most common failure of a getting-started page is
 * a paste that includes the prompt and fails with a shell error the reader cannot parse.
 */
export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);

  return (
    <button
      type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1600);
        } catch {
          // Clipboard can be blocked (insecure origin, permissions). Leave the label
          // unchanged rather than claiming success the reader did not get.
          setCopied(false);
        }
      }}
      className="shrink-0 rounded-md border border-zinc-700 px-2 py-1 font-mono text-[11px] text-zinc-400 transition hover:border-zinc-500 hover:text-zinc-200"
      aria-label={copied ? "Copied" : label}
    >
      {copied ? "✓ copied" : label}
    </button>
  );
}
