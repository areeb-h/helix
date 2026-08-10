import { Shell } from "@/components/Shell";
import api from "@/data/api.json";

// GENERATED FROM THE BINARY. `helix describe` emits the whole API as JSON — the same
// registry the interpreter dispatches on — so this page cannot list a method that does
// not exist, or omit one that does. Regenerate with `npm run sync:api`.
//
// This page is the fix for a real failure: a session probing the language concluded
// "Helix has no `scan`", a primitive that has existed since Stage 3t with a native
// kernel, and shipped an unnecessary O(n^2) fallback on that belief. A primitive that
// exists but cannot be found is half-missing.

interface Method {
  name: string;
  effect: string;
}

const EFFECT_STYLE: Record<string, string> = {
  pure: "text-zinc-500",
  io: "text-amber-400",
  net: "text-rose-400",
  rand: "text-fuchsia-400",
  time: "text-sky-400",
};

const TYPE_BLURB: Record<string, string> = {
  Array: "The workhorse. Comprehension verbs, statistics, and the packed numeric fast paths.",
  String: "Text, plus the parsing and formatting you would otherwise reach for a library to do.",
  Dna: "Nucleotide sequences as a first-class type — complement, k-mers, GC content.",
  DataFrame: "Columnar tables: select, filter, group, join, and read/write the usual formats.",
  GroupBy: "What a `group` gives you, before you aggregate it.",
  Dict: "Ordered key-value maps with O(log n) lookup (ADR 0020).",
  Record: "Structs with named fields, spread and update.",
  Net: "HTTP and sockets, behind the capability system.",
  Tensor: "N-dimensional numeric arrays and linear algebra.",
};

export default function ReferencePage() {
  const methods = api.methods as Record<string, Method[]>;
  const builtins = api.builtins as { name: string; category: string; effect: string }[];
  const types = Object.keys(methods).sort();
  const total = Object.values(methods).reduce((n, m) => n + m.length, 0);

  const byCategory = new Map<string, typeof builtins>();
  for (const b of builtins) {
    const list = byCategory.get(b.category) ?? [];
    list.push(b);
    byCategory.set(b.category, list);
  }
  const categories = [...byCategory.keys()].sort();

  const nav = [
    ...types.map((t) => ({ href: `#type-${t}`, label: `${t} (${methods[t].length})` })),
    ...categories.map((c) => ({ href: `#cat-${c}`, label: `${c} builtins` })),
  ];

  return (
    <Shell nav={nav} navTitle="Reference">
      <div className="max-w-3xl">
        <p className="font-mono text-xs uppercase tracking-widest text-emerald-500">
          Reference · Helix {api.helix_version}
        </p>
        <h1 className="mt-2 text-4xl font-bold tracking-tight">Every method, every builtin</h1>
        <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
          {total} methods across {types.length} types, and {builtins.length} builtins —
          generated from the binary itself with{" "}
          <code className="rounded bg-zinc-900 px-1.5 py-0.5 font-mono text-[13px] text-zinc-300">
            helix describe
          </code>
          , which emits the same registry the interpreter dispatches on. This page cannot
          list something that does not exist, and cannot omit something that does.
        </p>

        <div className="mt-6 rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
          <p className="text-[13px] leading-relaxed text-zinc-400">
            You have this offline too:{" "}
            <code className="text-zinc-300">helix doc Array</code> lists a type&apos;s
            methods, and <code className="text-zinc-300">helix describe</code> prints this
            whole document as JSON — built for tools and agents as much as for people.
          </p>
        </div>

        <div className="mt-8 flex flex-wrap gap-3 text-[12px]">
          {Object.entries(EFFECT_STYLE).map(([k, cls]) => (
            <span key={k} className="flex items-center gap-1.5">
              <span className={`font-mono ${cls}`}>●</span>
              <span className="text-zinc-500">{k}</span>
            </span>
          ))}
          <span className="text-zinc-600">— effects are tracked, not documented by hope.</span>
        </div>

        {types.map((t) => (
          <section key={t} id={`type-${t}`} className="mt-12 scroll-mt-24">
            <div className="flex items-baseline justify-between gap-4 border-b border-zinc-900 pb-2">
              <h2 className="text-2xl font-semibold tracking-tight">{t}</h2>
              <span className="shrink-0 font-mono text-[11px] text-zinc-600">
                {methods[t].length} methods
              </span>
            </div>
            {TYPE_BLURB[t] ? (
              <p className="mt-2 text-[14px] leading-relaxed text-zinc-500">{TYPE_BLURB[t]}</p>
            ) : null}
            <div className="mt-4 flex flex-wrap gap-1.5">
              {methods[t].map((m) => (
                <span
                  key={m.name}
                  title={`effect: ${m.effect}`}
                  className="rounded-md border border-zinc-800 bg-zinc-900/60 px-2 py-1 font-mono text-[12px] text-zinc-300"
                >
                  {m.name}
                  {m.effect !== "pure" ? (
                    <span className={`ml-1.5 ${EFFECT_STYLE[m.effect] ?? "text-zinc-500"}`}>
                      ●
                    </span>
                  ) : null}
                </span>
              ))}
            </div>
          </section>
        ))}

        <section className="mt-14">
          <h2 className="text-2xl font-semibold tracking-tight">Builtins</h2>
          <p className="mt-2 text-[14px] leading-relaxed text-zinc-500">
            Free functions, grouped by what they touch. Anything not marked{" "}
            <span className="text-zinc-400">pure</span> reaches the outside world.
          </p>
          {categories.map((c) => (
            <div key={c} id={`cat-${c}`} className="mt-8 scroll-mt-24">
              <h3 className="font-mono text-[12px] uppercase tracking-widest text-emerald-500">
                {c}
              </h3>
              <div className="mt-2.5 flex flex-wrap gap-1.5">
                {byCategory.get(c)!.map((b) => (
                  <span
                    key={b.name}
                    title={`effect: ${b.effect}`}
                    className="rounded-md border border-zinc-800 bg-zinc-900/60 px-2 py-1 font-mono text-[12px] text-zinc-300"
                  >
                    {b.name}
                    {b.effect !== "pure" ? (
                      <span className={`ml-1.5 ${EFFECT_STYLE[b.effect] ?? "text-zinc-500"}`}>
                        ●
                      </span>
                    ) : null}
                  </span>
                ))}
              </div>
            </div>
          ))}
        </section>

        <section className="mt-14 rounded-xl border border-zinc-800 bg-zinc-900/40 p-5">
          <h3 className="font-semibold text-zinc-100">Universal</h3>
          <p className="mt-1.5 text-[13px] text-zinc-500">
            Available on every value, whatever its type.
          </p>
          <div className="mt-3 flex flex-wrap gap-1.5">
            {(api.universal_methods as string[]).map((m) => (
              <span
                key={m}
                className="rounded-md border border-zinc-800 bg-zinc-900/60 px-2 py-1 font-mono text-[12px] text-zinc-300"
              >
                {m}
              </span>
            ))}
          </div>
        </section>
      </div>
    </Shell>
  );
}
