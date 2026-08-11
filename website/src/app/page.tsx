import Link from "next/link";
import { Code } from "@/components/Code";
import { CopyButton } from "@/components/Copy";

// Every claim on this page is measured and commit-traceable. The rule the language holds
// itself to — never publish a number you have not just measured — applies to its own
// marketing, which is the only version of that rule that means anything.

const SAMPLE = `# One file. No imports, no main, no boilerplate.
reads = [
    {id: "r1", seq: dna("ATGCGC"), q: 38},
    {id: "r2", seq: dna("ATTTTA"), q: 12},
    {id: "r3", seq: dna("GCGCGC"), q: 41},
]

kept = reads.where(r => r.q >= 30)
print("kept {kept.count()} of {reads.count()} reads")
print("GC content: {kept.map(r => r.seq.gc_content()).mean()}")`;

export default function Home() {
  return (
    <div className="min-h-screen bg-zinc-950 font-sans text-zinc-100">
      <header className="sticky top-0 z-10 border-b border-zinc-900 bg-zinc-950/85 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
          <div className="flex items-baseline gap-2">
            <span className="text-lg font-bold tracking-tight text-emerald-400">helix</span>
            <span className="hidden text-xs text-zinc-500 sm:inline">
              three engines, one answer
            </span>
          </div>
          <nav className="flex items-center gap-5 text-sm text-zinc-400">
            <Link href="/why" className="hover:text-zinc-100">Why</Link>
            <Link href="/start" className="hover:text-zinc-100">Get started</Link>
            <Link href="/tour" className="hidden hover:text-zinc-100 md:inline">Tour</Link>
            <Link href="/reference" className="hidden hover:text-zinc-100 md:inline">Reference</Link>
            <Link href="/docs" className="hover:text-zinc-100">Docs</Link>
            <a
              href="https://github.com/areeb-h/helix"
              className="rounded-lg border border-zinc-700 px-3 py-1.5 hover:border-zinc-500 hover:text-zinc-100"
            >
              GitHub
            </a>
          </nav>
        </div>
      </header>

      <main className="mx-auto max-w-6xl px-6">
        {/* hero */}
        <section className="grid items-center gap-12 py-20 lg:grid-cols-2">
          <div>
            <h1 className="text-5xl font-bold leading-[1.1] tracking-tight">
              A scientific language
              <br />
              <span className="text-emerald-400">that shows its work.</span>
            </h1>
            <p className="mt-6 text-lg leading-relaxed text-zinc-400">
              Helix runs every program three ways — a tree-walking interpreter, a bytecode
              VM, and a Cranelift JIT — and holds them{" "}
              <span className="text-zinc-200">bit-identical</span>, values and error text
              alike, on every commit. A wrong answer has to be wrong three times, in three
              independent implementations, to reach you.
            </p>

            <div className="mt-8 flex flex-wrap items-center gap-3">
              <Link
                href="/start"
                className="rounded-lg bg-emerald-500 px-5 py-2.5 text-sm font-semibold text-zinc-950 transition hover:bg-emerald-400"
              >
                Get started →
              </Link>
              <Link
                href="/why"
                className="rounded-lg border border-zinc-700 px-5 py-2.5 text-sm font-semibold text-zinc-300 transition hover:border-zinc-500"
              >
                Why three engines?
              </Link>
            </div>

            <p className="mt-5 text-[13px] leading-relaxed text-zinc-500">
              Helix loses to single-threaded C on seven of nine benchmark kernels, and{" "}
              <Link
                href="/bench"
                className="text-zinc-400 underline decoration-zinc-700 underline-offset-2 hover:text-emerald-400"
              >
                the benchmark page says so first
              </Link>
              . A suite that only reports wins is marketing.
            </p>

            <div className="mt-6 flex items-center gap-2 rounded-lg border border-zinc-800 bg-zinc-900/60 px-3 py-2">
              <code className="flex-1 font-mono text-[13px] text-zinc-300">
                <span className="select-none text-emerald-500">$ </span>
                helix eval &quot;print([1, 2, 3].sum())&quot;
              </code>
              <CopyButton text='helix eval "print([1, 2, 3].sum())"' />
            </div>
          </div>

          <div>
            <div className="mb-2 flex items-center justify-between">
              <span className="font-mono text-[11px] text-zinc-600">reads.helix</span>
              <span className="rounded-full border border-emerald-800 bg-emerald-950/70 px-2.5 py-0.5 font-mono text-[11px] text-emerald-400">
                ✓ output verified on 3 engines
              </span>
            </div>
            <Code source={SAMPLE} />
            <div className="mt-2 rounded-lg border border-zinc-800 bg-zinc-950 p-3 font-mono text-[12px] leading-6 text-zinc-400">
              <div>kept 2 of 3 reads</div>
              <div>GC content: 0.8333333333333333</div>
            </div>
          </div>
        </section>

        {/* the three claims, each with a receipt */}
        <section className="grid gap-4 pb-6 md:grid-cols-3">
          {[
            {
              t: "Docs that cannot rot",
              b: "Every documented example is extracted and executed on all three engines, every CI run. A doc whose output has drifted fails the build.",
              l: "/tour",
              c: "See the tour →",
            },
            {
              t: "Fast where it counts",
              b: "Comprehensions compile to native kernels over packed columns. Ten kernels benchmarked against C, Rust, Go, CPython and NumPy — losses published alongside wins.",
              l: "/bench",
              c: "See the numbers →",
            },
            {
              t: "Missing data, done right",
              b: "One `missing` marker that propagates through arithmetic and reductions instead of silently becoming zero. Dropping it is a deliberate, visible step.",
              l: "/docs",
              c: "Read the docs →",
            },
          ].map((f) => (
            <Link
              key={f.t}
              href={f.l}
              className="group rounded-xl border border-zinc-800 bg-zinc-900/40 p-5 transition hover:border-emerald-900 hover:bg-zinc-900/70"
            >
              <h3 className="font-semibold text-zinc-100 group-hover:text-emerald-300">
                {f.t}
              </h3>
              <p className="mt-2 text-sm leading-relaxed text-zinc-400">{f.b}</p>
              <span className="mt-3 inline-block font-mono text-[11px] text-emerald-500">
                {f.c}
              </span>
            </Link>
          ))}
        </section>

        {/* why three engines — the actual differentiator, explained */}
        <section className="border-t border-zinc-900 py-16">
          <div className="grid gap-10 lg:grid-cols-[1fr_1.1fr]">
            <div>
              <h2 className="text-3xl font-bold tracking-tight">
                Why three engines?
              </h2>
              <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
                Because a fast language that is quietly wrong is worse than a slow one. A
                JIT is thousands of lines of code generation between your program and your
                answer — so Helix keeps two simpler implementations alongside it and
                requires all three to agree, byte for byte, on values{" "}
                <em>and on error messages</em>.
              </p>
              <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
                It works. The oracle has caught real defects that shipped nowhere: a
                signed-zero comparison that made a kernel disagree with the interpreter, an
                integer subexpression that wrapped in one engine and not another. Each was
                found the same way — three answers, one of them different.
              </p>
            </div>
            <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-6">
              <div className="font-mono text-[11px] uppercase tracking-widest text-zinc-600">
                the same program, three ways
              </div>
              <div className="mt-4 space-y-3 font-mono text-[13px]">
                {[
                  ["helix run x.helix", "JIT — native code"],
                  ["HELIX_NOJIT=1 helix run x.helix", "bytecode VM"],
                  ["HELIX_NOVM=1 helix run x.helix", "tree-walking interpreter"],
                ].map(([cmd, what]) => (
                  <div key={cmd} className="flex flex-col gap-0.5">
                    <code className="text-zinc-300">
                      <span className="select-none text-emerald-500">$ </span>
                      {cmd}
                    </code>
                    <span className="pl-3.5 text-[11px] text-zinc-600">{what}</span>
                  </div>
                ))}
              </div>
              <div className="mt-4 border-t border-zinc-800 pt-3 text-[12px] text-zinc-500">
                Identical output required. Any divergence is a bug, and the test suite
                treats it as one.
              </div>
            </div>
          </div>
        </section>

        {/* closing */}
        <section className="border-t border-zinc-900 py-16 text-center">
          <h2 className="text-3xl font-bold tracking-tight">Ten minutes to a shipped binary</h2>
          <p className="mx-auto mt-4 max-w-xl text-[15px] leading-relaxed text-zinc-400">
            Install, hello world, a real program, then{" "}
            <code className="rounded bg-zinc-900 px-1.5 py-0.5 font-mono text-[13px] text-zinc-300">
              helix build
            </code>{" "}
            — a standalone executable that runs where nothing is installed.
          </p>
          <Link
            href="/start"
            className="mt-7 inline-block rounded-lg bg-emerald-500 px-6 py-3 text-sm font-semibold text-zinc-950 transition hover:bg-emerald-400"
          >
            Get started →
          </Link>
        </section>
      </main>

      <footer className="border-t border-zinc-900 py-8 text-center text-xs text-zinc-600">
        Helix {" "}
        <span className="text-zinc-700">·</span> every example on this site is executed by
        the test gate on all three engines
      </footer>
    </div>
  );
}
