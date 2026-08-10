// Landing page. Every number on this page is a MEASURED result from the repo's own
// benchmark records (bench/kernels/RESULTS.md, docs/ROADMAP.md) — the site inherits the
// project's no-overclaiming rule: anything stated here must be traceable to a commit.
// The code sample is a real, doc-tested example from examples/language/.
export default function Home() {
  return (
    <div className="min-h-screen bg-zinc-950 font-sans text-zinc-100">
      <header className="mx-auto flex max-w-5xl items-center justify-between px-6 py-5">
        <div className="flex items-baseline gap-2">
          <span className="text-xl font-bold tracking-tight text-emerald-400">helix</span>
          <span className="text-xs text-zinc-500">a scientific language you can trust</span>
        </div>
        <nav className="flex gap-6 text-sm text-zinc-400">
          <a href="/docs" className="hover:text-zinc-100">Docs</a>
          <a href="/tour" className="hover:text-zinc-100">Tour</a>
          <a href="/bench" className="hover:text-zinc-100">Benchmarks</a>
        </nav>
      </header>

      <main className="mx-auto max-w-5xl px-6">
        <section className="py-20">
          <h1 className="max-w-3xl text-5xl font-bold leading-tight tracking-tight">
            Three engines. <span className="text-emerald-400">One answer.</span>
          </h1>
          <p className="mt-6 max-w-2xl text-lg leading-relaxed text-zinc-400">
            Helix runs every program on a tree-walking interpreter, a bytecode VM, and a
            Cranelift JIT — and holds them{" "}
            <span className="text-zinc-200">bit-identical</span>, values and error text
            alike, in CI. When your result matters, the differential oracle is not a test
            suite. It is the language.
          </p>
          <div className="mt-8 flex gap-3">
            <a
              href="/docs"
              className="rounded-lg bg-emerald-500 px-5 py-2.5 text-sm font-semibold text-zinc-950 hover:bg-emerald-400"
            >
              Read the docs
            </a>
            <a
              href="/tour"
              className="rounded-lg border border-zinc-700 px-5 py-2.5 text-sm font-semibold text-zinc-300 hover:border-zinc-500"
            >
              Take the tour
            </a>
          </div>
        </section>

        <section className="grid gap-4 pb-16 md:grid-cols-3">
          {[
            {
              title: "Docs that cannot lie",
              body: "Every documented example is extracted and executed on all three engines, every CI run. A doc whose example has drifted fails the build — what you read is what runs.",
            },
            {
              title: "Fast where it counts",
              body: "Comprehensions compile to native kernels over packed columns — no boxing. Measured on the repo's own honest harness: 13–26× filters, 361× histograms, ahead of NumPy on its own matmul benchmark.",
            },
            {
              title: "Missing data, done right",
              body: "One `missing` marker, ADR-governed: it propagates through arithmetic and reductions instead of silently becoming zero. Dropping it is a visible, deliberate step.",
            },
          ].map((f) => (
            <div
              key={f.title}
              className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-5"
            >
              <h3 className="font-semibold text-zinc-100">{f.title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-zinc-400">{f.body}</p>
            </div>
          ))}
        </section>

        <section className="pb-24">
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/70 p-6 font-mono text-sm leading-7">
            <div className="mb-3 flex items-center justify-between">
              <span className="text-xs uppercase tracking-widest text-zinc-500">
                examples/language/collections.helix
              </span>
              <span className="rounded-full border border-emerald-800 bg-emerald-950 px-2.5 py-0.5 text-xs text-emerald-400">
                ✓ executed on 3 engines in CI
              </span>
            </div>
            <pre className="overflow-x-auto text-zinc-300">{`## scan is reduce that KEEPS every intermediate accumulator —
## the primitive for any "this depends on the previous one" recurrence.
##
##     >>> [1, 2, 3].scan(0, (acc, x) => acc + x)
##     [1, 3, 6]

xs = [42, 8, 15, 16, 23, 4]
print("evens:", xs.filter(it % 2 == 0))
print("sum:  ", xs.reduce(0, (acc, x) => acc + x))`}</pre>
          </div>
          <p className="mt-3 text-xs text-zinc-500">
            The badge is not decoration: this block is a real `##` doc-test from the
            repository, and the gate refuses any example whose output has drifted.
          </p>
        </section>
      </main>

      <footer className="border-t border-zinc-900 py-8 text-center text-xs text-zinc-600">
        Helix — built with a differential oracle, benchmarked honestly.
      </footer>
    </div>
  );
}
