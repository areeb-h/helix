import { Shell } from "@/components/Shell";
import { readRepoFile } from "@/lib/content";
import { renderMarkdown } from "@/lib/markdown";

// The benchmark page is bench/kernels/RESULTS.md, rendered whole — caveats, corrections,
// losses and all. That is the point: the file already contains the honest account
// (including a published number that was later found wrong and corrected), and excerpting
// it into a highlight reel is exactly the failure this project refuses.
export default function BenchPage() {
  const md = readRepoFile("bench/kernels/RESULTS.md");

  return (
    <Shell>
      <div className="max-w-4xl">
        <div className="mb-8">
          <h1 className="text-4xl font-bold tracking-tight">Benchmarks</h1>
          <p className="mt-4 max-w-2xl text-zinc-400">
            Ten kernels against single-threaded C, Rust, Go, CPython and NumPy — the whole
            record, wins and losses, rendered directly from{" "}
            <code className="rounded bg-zinc-900 px-1.5 py-0.5 font-mono text-[13px] text-zinc-300">
              bench/kernels/RESULTS.md
            </code>
            .
          </p>
        </div>

        <div className="mb-8 grid gap-3 sm:grid-cols-3">
          {[
            {
              k: "Anchored",
              v: "Byte-identical output",
              d: "Every language of a kernel must print the same bytes before a timing counts. A faster program computing something else is not a benchmark.",
            },
            {
              k: "%CPU reported",
              v: "Cores are not free",
              d: "Helix parallelizes some kernels; the references never do. A wall-clock win bought with 2.8× the cores is labelled as such, not banked.",
            },
            {
              k: "Corrections kept",
              v: "Including our own",
              d: "A published k7 ratio was measured against a stale binary and was wrong. The correction stays in the file rather than being quietly replaced.",
            },
          ].map((c) => (
            <div key={c.k} className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
              <div className="text-[11px] font-semibold uppercase tracking-widest text-emerald-500">
                {c.k}
              </div>
              <div className="mt-1 font-semibold text-zinc-100">{c.v}</div>
              <p className="mt-1.5 text-[13px] leading-relaxed text-zinc-500">{c.d}</p>
            </div>
          ))}
        </div>

        <div
          className="prose-helix"
          dangerouslySetInnerHTML={{ __html: renderMarkdown(md) }}
        />
      </div>
    </Shell>
  );
}
