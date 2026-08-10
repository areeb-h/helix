import { Shell } from "@/components/Shell";
import { Code, VerifiedExample } from "@/components/Code";
import { listExamples } from "@/lib/content";
import { extractDocBlocks } from "@/lib/helix";

// The tour is GENERATED from examples/language/*.helix — the same files the gate runs
// end-to-end and whose `##` examples it executes on all three engines. Nothing here is
// written twice, so nothing here can drift from the language.
export default function TourPage() {
  const examples = listExamples();
  const nav = examples.map((e) => ({ href: `#${e.slug}`, label: e.title }));

  return (
    <Shell nav={nav} navTitle="The tour">
      <div className="max-w-3xl">
        <h1 className="text-4xl font-bold tracking-tight">The tour</h1>
        <p className="mt-4 text-zinc-400">
          Every page below is a real file in{" "}
          <code className="rounded bg-zinc-900 px-1.5 py-0.5 font-mono text-[13px] text-zinc-300">
            examples/language/
          </code>
          . The gate runs each one on the tree-walking interpreter, the bytecode VM and
          the JIT, and requires all three to agree byte-for-byte — so this tour cannot
          describe a language that does not exist.
        </p>

        {examples.map((ex) => {
          const blocks = extractDocBlocks(ex.source);
          const withExamples = blocks.filter((b) => b.examples.length > 0);
          return (
            <section key={ex.slug} id={ex.slug} className="mt-14 scroll-mt-24">
              <div className="flex items-baseline justify-between gap-4">
                <h2 className="text-2xl font-semibold tracking-tight">{ex.title}</h2>
                <code className="shrink-0 font-mono text-[11px] text-zinc-600">
                  {ex.rel}
                </code>
              </div>

              {ex.intro.length > 0 ? (
                <p className="mt-3 whitespace-pre-wrap text-[15px] leading-relaxed text-zinc-400">
                  {ex.intro.join("\n")}
                </p>
              ) : null}

              {withExamples.length > 0 ? (
                <div className="mt-5">
                  {withExamples.map((b, bi) => (
                    <div key={bi}>
                      {b.prose.length > 0 ? (
                        <p className="mt-6 whitespace-pre-wrap text-[15px] leading-relaxed text-zinc-300">
                          {b.prose.join("\n").trim()}
                        </p>
                      ) : null}
                      {b.examples.map((e, ei) => (
                        <VerifiedExample
                          key={ei}
                          code={e.code}
                          expect={e.expect}
                          rel={ex.rel}
                          line={e.line}
                        />
                      ))}
                      {b.code ? <Code source={b.code} /> : null}
                    </div>
                  ))}
                </div>
              ) : (
                <Code source={stripLeadingComment(ex.source)} className="mt-5" />
              )}
            </section>
          );
        })}
      </div>
    </Shell>
  );
}

/** Drop the file's header comment — it is already rendered as the section intro. */
function stripLeadingComment(src: string): string {
  const lines = src.split("\n");
  let i = 0;
  while (i < lines.length && (lines[i].trimStart().startsWith("#") || lines[i].trim() === "")) {
    i++;
  }
  return lines.slice(i).join("\n").trim();
}
