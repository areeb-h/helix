"use client";

import { useState } from "react";

export interface Snippet {
  name: string;
  source: string;
}

export function Playground({ snippets }: { snippets: Snippet[] }) {
  const [source, setSource] = useState(snippets[0]?.source ?? "");
  const [out, setOut] = useState<{
    stdout?: string;
    stderr?: string;
    error?: string;
    detail?: string;
    refused?: boolean;
    disabled?: boolean;
    timedOut?: boolean;
  } | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async () => {
    setBusy(true);
    setOut(null);
    try {
      const res = await fetch("/api/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ source }),
      });
      setOut(await res.json());
    } catch (e) {
      setOut({ error: e instanceof Error ? e.message : "request failed" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <div className="mb-3 flex flex-wrap gap-2">
        {snippets.map((s) => (
          <button
            key={s.name}
            type="button"
            onClick={() => {
              setSource(s.source);
              setOut(null);
            }}
            className="rounded-md border border-zinc-800 bg-zinc-900/60 px-2.5 py-1 font-mono text-[12px] text-zinc-400 transition hover:border-emerald-800 hover:text-emerald-300"
          >
            {s.name}
          </button>
        ))}
      </div>

      <div className="grid gap-3 lg:grid-cols-2">
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <div className="flex items-center justify-between border-b border-zinc-800 bg-zinc-900/70 px-3 py-1.5">
            <span className="font-mono text-[11px] text-zinc-500">main.helix</span>
            <button
              type="button"
              onClick={run}
              disabled={busy}
              className="rounded-md bg-emerald-500 px-3 py-1 font-mono text-[11px] font-semibold text-zinc-950 transition hover:bg-emerald-400 disabled:opacity-50"
            >
              {busy ? "running…" : "▶ run"}
            </button>
          </div>
          <textarea
            value={source}
            onChange={(e) => setSource(e.target.value)}
            spellCheck={false}
            rows={16}
            className="w-full resize-y bg-zinc-950 p-4 font-mono text-[13px] leading-6 text-zinc-200 outline-none"
          />
        </div>

        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <div className="border-b border-zinc-800 bg-zinc-900/70 px-3 py-1.5">
            <span className="font-mono text-[11px] text-zinc-500">output</span>
          </div>
          <div className="min-h-[16rem] bg-zinc-950 p-4 font-mono text-[13px] leading-6">
            {out === null ? (
              <span className="text-zinc-600">
                {busy ? "…" : "Press run."}
              </span>
            ) : out.disabled ? (
              <div className="text-zinc-400">
                <div className="text-amber-400">The playground is off on this deployment.</div>
                <p className="mt-2 whitespace-pre-wrap font-sans text-[13px] leading-relaxed text-zinc-500">
                  Running submitted code needs the Helix binary and an explicit opt-in
                  (<code className="text-zinc-400">HELIX_PLAYGROUND=1</code>), so a static
                  deployment does not inherit remote code execution by accident. It works
                  locally the moment you clone the repo and run the dev server.
                </p>
              </div>
            ) : out.refused ? (
              <div>
                <div className="text-amber-400">{out.error}</div>
                <p className="mt-2 whitespace-pre-wrap font-sans text-[13px] leading-relaxed text-zinc-500">
                  {out.detail}
                </p>
              </div>
            ) : (
              <>
                {out.stdout ? (
                  <pre className="whitespace-pre-wrap text-zinc-200">{out.stdout}</pre>
                ) : null}
                {out.stderr ? (
                  <pre className="mt-2 whitespace-pre-wrap text-rose-400">{out.stderr}</pre>
                ) : null}
                {out.error ? <div className="text-rose-400">{out.error}</div> : null}
                {!out.stdout && !out.stderr && !out.error ? (
                  <span className="text-zinc-600">(no output)</span>
                ) : null}
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
