"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import type { SearchEntry } from "@/lib/search";

/**
 * Scoring is deliberately simple and explainable rather than fuzzy-clever: an exact
 * name match must win, because the question this box mostly answers is "does Helix have
 * X?" — and a fuzzy matcher that buries `scan()` under a prose paragraph mentioning
 * "scanning" would recreate the exact discoverability failure this site exists to fix.
 */
function score(entry: SearchEntry, q: string): number {
  const title = entry.title.toLowerCase();
  const body = entry.body.toLowerCase();
  const bare = title.replace(/\(\)$/, "");

  if (bare === q) return 1000;
  if (title.startsWith(q)) return 700 - title.length;
  if (bare.includes(q)) return 400 - title.length;
  if (entry.kind.toLowerCase().includes(q)) return 200;
  if (body.includes(q)) return 100;
  return -1;
}

export function SearchDialog({ index }: { index: SearchEntry[] }) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const router = useRouter();

  const results = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return [];
    return index
      .map((e) => ({ e, s: score(e, needle) }))
      .filter((r) => r.s >= 0)
      .sort((a, b) => b.s - a.s)
      .slice(0, 24)
      .map((r) => r.e);
  }, [q, index]);

  // Cmd/Ctrl-K opens; `/` opens unless you are already typing somewhere.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing =
        document.activeElement instanceof HTMLInputElement ||
        document.activeElement instanceof HTMLTextAreaElement;
      if ((e.key === "k" && (e.metaKey || e.ctrlKey)) || (e.key === "/" && !typing)) {
        e.preventDefault();
        setOpen(true);
      }
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    if (open) {
      setSel(0);
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  useEffect(() => setSel(0), [q]);

  const go = (href: string) => {
    setOpen(false);
    setQ("");
    router.push(href);
  };

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="flex items-center gap-2 rounded-lg border border-zinc-800 bg-zinc-900/60 px-3 py-1.5 text-sm text-zinc-500 transition hover:border-zinc-700 hover:text-zinc-300"
        aria-label="Search"
      >
        <span>Search</span>
        <kbd className="rounded border border-zinc-700 px-1.5 font-mono text-[10px] text-zinc-500">
          ⌘K
        </kbd>
      </button>

      {open ? (
        <div
          className="fixed inset-0 z-50 flex items-start justify-center bg-black/70 p-4 pt-[12vh]"
          onClick={() => setOpen(false)}
        >
          <div
            className="w-full max-w-xl overflow-hidden rounded-xl border border-zinc-800 bg-zinc-950 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <input
              ref={inputRef}
              value={q}
              onChange={(e) => setQ(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setSel((s) => Math.min(s + 1, results.length - 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setSel((s) => Math.max(s - 1, 0));
                } else if (e.key === "Enter" && results[sel]) {
                  e.preventDefault();
                  go(results[sel].href);
                }
              }}
              placeholder="Search methods, docs, examples…  (try: scan)"
              className="w-full border-b border-zinc-800 bg-transparent px-4 py-3.5 text-[15px] text-zinc-100 outline-none placeholder:text-zinc-600"
            />

            <div className="max-h-[55vh] overflow-y-auto">
              {q.trim() === "" ? (
                <div className="px-4 py-6 text-center text-sm text-zinc-600">
                  Every method, builtin, doc heading and example — searchable.
                </div>
              ) : results.length === 0 ? (
                <div className="px-4 py-6 text-center text-sm text-zinc-500">
                  Nothing matches{" "}
                  <span className="font-mono text-zinc-300">{q}</span>. If you expected a
                  method here, it genuinely does not exist — this index is generated from
                  the binary.
                </div>
              ) : (
                results.map((r, i) => (
                  <button
                    key={`${r.href}-${r.title}-${i}`}
                    type="button"
                    onMouseEnter={() => setSel(i)}
                    onClick={() => go(r.href)}
                    className={`flex w-full items-baseline gap-3 border-b border-zinc-900 px-4 py-2.5 text-left ${
                      i === sel ? "bg-zinc-900" : ""
                    }`}
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block truncate font-mono text-[13px] text-zinc-100">
                        {r.title}
                      </span>
                      {r.body ? (
                        <span className="block truncate text-[12px] text-zinc-500">
                          {r.body}
                        </span>
                      ) : null}
                    </span>
                    <span className="shrink-0 rounded border border-zinc-800 px-1.5 py-0.5 font-mono text-[10px] text-zinc-500">
                      {r.kind}
                    </span>
                  </button>
                ))
              )}
            </div>

            <div className="flex items-center gap-4 border-t border-zinc-900 px-4 py-2 font-mono text-[10px] text-zinc-600">
              <span>↑↓ navigate</span>
              <span>↵ open</span>
              <span>esc close</span>
              <span className="ml-auto">{results.length ? `${results.length} results` : ""}</span>
            </div>
          </div>
        </div>
      ) : null}
    </>
  );
}
