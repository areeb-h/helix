import { tokenize, type TokenKind } from "@/lib/helix";

const CLASS: Record<TokenKind, string> = {
  doc: "text-emerald-300/70 italic",
  comment: "text-zinc-500",
  string: "text-amber-300",
  interp: "text-amber-200 font-medium",
  number: "text-sky-300",
  keyword: "text-fuchsia-400",
  builtin: "text-cyan-300",
  method: "text-emerald-300",
  operator: "text-zinc-400",
  punct: "text-zinc-500",
  ident: "text-zinc-200",
  plain: "text-zinc-300",
};

export function Code({ source, className = "" }: { source: string; className?: string }) {
  const tokens = tokenize(source);
  return (
    <pre
      className={`overflow-x-auto rounded-lg border border-zinc-800 bg-zinc-900/70 p-4 font-mono text-[13px] leading-6 ${className}`}
    >
      <code>
        {tokens.map((t, i) => (
          <span key={i} className={CLASS[t.kind]}>
            {t.text}
          </span>
        ))}
      </code>
    </pre>
  );
}

/**
 * A documented example, rendered as the gate sees it: the program, then the output the
 * repository asserts it produces.
 *
 * The badge is the site's whole differentiator, so it must not become decoration. It
 * claims exactly one thing — that `doc_examples_run_and_agree_on_all_three_engines`
 * extracts THIS example from THIS file and requires the output below — and the extractor
 * here mirrors the gate's. Examples with no expected output get no badge, because there
 * is nothing for the gate to have checked.
 */
export function VerifiedExample({
  code,
  expect,
  rel,
  line,
}: {
  code: string[];
  expect: string[];
  rel: string;
  line: number;
}) {
  const verified = expect.length > 0;
  return (
    <figure className="my-5 overflow-hidden rounded-xl border border-zinc-800">
      <figcaption className="flex items-center justify-between gap-3 border-b border-zinc-800 bg-zinc-900/60 px-4 py-2">
        <code className="truncate font-mono text-[11px] text-zinc-500">
          {rel}:{line}
        </code>
        {verified ? (
          <span
            className="shrink-0 rounded-full border border-emerald-800 bg-emerald-950/70 px-2.5 py-0.5 font-mono text-[11px] text-emerald-400"
            title="Extracted and executed by doc_examples_run_and_agree_on_all_three_engines in tests/cli.rs, on the tree-walker, the bytecode VM and the JIT. If this output drifts, the build fails."
          >
            ✓ verified on 3 engines
          </span>
        ) : null}
      </figcaption>
      <div className="bg-zinc-950/60 p-4 font-mono text-[13px] leading-6">
        {code.map((l, i) => (
          <div key={i} className="flex gap-3">
            <span className="select-none text-zinc-600">&gt;&gt;&gt;</span>
            <span className="min-w-0 flex-1 overflow-x-auto">
              <CodeInline source={l} />
            </span>
          </div>
        ))}
        {verified ? (
          <div className="mt-2 border-t border-zinc-800/80 pt-2 text-zinc-400">
            {expect.map((l, i) => (
              <div key={i} className="whitespace-pre-wrap">
                {l}
              </div>
            ))}
          </div>
        ) : null}
      </div>
    </figure>
  );
}

function CodeInline({ source }: { source: string }) {
  const tokens = tokenize(source);
  return (
    <>
      {tokens.map((t, i) => (
        <span key={i} className={CLASS[t.kind]}>
          {t.text}
        </span>
      ))}
    </>
  );
}
