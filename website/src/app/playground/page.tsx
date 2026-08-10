import Link from "next/link";
import { Shell } from "@/components/Shell";
import { Playground } from "@/components/Playground";

export default function PlaygroundPage() {
  return (
    <Shell>
      <div className="max-w-5xl">
        <p className="font-mono text-xs uppercase tracking-widest text-emerald-500">
          Playground
        </p>
        <h1 className="mt-2 text-4xl font-bold tracking-tight">Try it</h1>
        <p className="mt-4 max-w-2xl text-[15px] leading-relaxed text-zinc-400">
          Edit and run. Programs execute on a real Helix binary — the same one the test
          gate uses — with a four-second limit.
        </p>

        <div className="mt-8">
          <Playground snippets={SNIPPETS} />
        </div>

        <div className="mt-8 grid max-w-3xl gap-4 sm:grid-cols-2">
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
            <h3 className="text-sm font-semibold text-zinc-100">
              Why computation only?
            </h3>
            <p className="mt-2 text-[13px] leading-relaxed text-zinc-500">
              Helix records, for every builtin and method, whether it is pure and which
              capability it needs — filesystem, network, the clock. This endpoint refuses
              anything impure using that registry rather than a hand-maintained
              blocklist, so it cannot drift: a networking builtin added tomorrow is
              refused the day it exists. Nondeterminism like <code>shuffle</code> is
              allowed — it reaches nothing outside the process.
            </p>
          </div>
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
            <h3 className="text-sm font-semibold text-zinc-100">Why not in the browser?</h3>
            <p className="mt-2 text-[13px] leading-relaxed text-zinc-500">
              A WebAssembly build would be better — no server, no limits. It is not
              possible yet: the JIT needs executable memory, and the DataFrame engine
              pulls in an async runtime. Both are unconditional dependencies today, so
              that is a real engine task, not a switch.
            </p>
          </div>
        </div>

        <p className="mt-8 max-w-2xl text-[13px] leading-relaxed text-zinc-500">
          For the whole language — files, DataFrames, HTTP, charts —{" "}
          <Link href="/start" className="text-emerald-400 underline-offset-2 hover:underline">
            install Helix
          </Link>
          . It is one self-contained binary and it takes a minute.
        </p>
      </div>
    </Shell>
  );
}

// Snippets chosen to show something a reader would not guess: euclidean modulo, the
// three-valued `missing`, DNA as a first-class type, and `scan` — the primitive whose
// undiscoverability cost a downstream project a wrong design decision.
const SNIPPETS = [
  {
    name: "hello",
    source: `print("Hello, Helix!")

xs = [42, 8, 15, 16, 23, 4]
print("mean: {xs.mean()}")
print("sorted: {xs.sort()}")`,
  },
  {
    name: "euclidean %",
    source: `# Modulo and floor-division are EUCLIDEAN — the remainder is never
# negative. This surprises everyone arriving from C, Rust, Go or JS.
print(7 % 3)
print(-7 % 3)
print(7 % -3)
print(-7 // 3)`,
  },
  {
    name: "missing",
    source: `# One marker for absent data. It PROPAGATES rather than
# silently becoming zero — dropping it is a visible step.
xs = [1, missing, 3]
print(xs.sum())
print(xs.drop_missing().sum())
print(missing ?? 30)`,
  },
  {
    name: "dna",
    source: `# DNA is a first-class type, not a string convention.
seq = dna("ATGCGTAC")
print(seq.gc_content())
print(seq.reverse_complement())
print(seq.kmers(3))`,
  },
  {
    name: "scan",
    source: `# scan is reduce that KEEPS every intermediate — the
# primitive for any "depends on the previous one" recurrence.
print([1, 2, 3, 4].scan(0, (acc, x) => acc + x))

# Greedy pairing of a run: flag adjacent equal pairs, then scan.
s = [1, 1, 1, 1, 1]
flags = s.zip(s.drop(1)).map((x, y) => if x == 1 and y == 1 then 1 else 0)
print(flags.scan(0, (prev, f) => if f == 1 and prev == 0 then 1 else 0))`,
  },
];
