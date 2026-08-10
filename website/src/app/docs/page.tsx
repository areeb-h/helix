import Link from "next/link";
import { Shell } from "@/components/Shell";
import { listDocs } from "@/lib/content";

export default function DocsIndex() {
  const docs = listDocs();
  const nav = docs.map((d) => ({ href: `/docs/${d.slug}`, label: d.title }));

  return (
    <Shell nav={nav} navTitle="Documentation">
      <div className="max-w-3xl">
        <h1 className="text-4xl font-bold tracking-tight">Documentation</h1>
        <p className="mt-4 text-zinc-400">
          These pages are the repository&apos;s own documents, rendered directly from{" "}
          <code className="rounded bg-zinc-900 px-1.5 py-0.5 font-mono text-[13px] text-zinc-300">
            docs/
          </code>
          . They are written for people who need to know what the language actually does
          — including where it is slow, and what is still open.
        </p>

        <div className="mt-10 grid gap-3 sm:grid-cols-2">
          {docs.map((d) => (
            <Link
              key={d.slug}
              href={`/docs/${d.slug}`}
              className="group rounded-xl border border-zinc-800 bg-zinc-900/40 p-5 hover:border-emerald-800 hover:bg-zinc-900/70"
            >
              <div className="font-semibold text-zinc-100 group-hover:text-emerald-300">
                {d.title}
              </div>
              <div className="mt-1 font-mono text-[11px] text-zinc-600">{d.rel}</div>
            </Link>
          ))}
        </div>

        <div className="mt-10 rounded-xl border border-emerald-900/60 bg-emerald-950/20 p-5">
          <div className="font-semibold text-emerald-300">Start with the tour</div>
          <p className="mt-2 text-sm leading-relaxed text-zinc-400">
            If you want the language rather than its internals, the{" "}
            <Link href="/tour" className="text-emerald-400 underline-offset-2 hover:underline">
              tour
            </Link>{" "}
            is generated from runnable example files, and every example on it is executed
            on all three engines by the test gate.
          </p>
        </div>
      </div>
    </Shell>
  );
}
