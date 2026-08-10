import { CopyButton } from "./Copy";

export interface Step {
  /** Shell commands, without the `$`. */
  cmd: string[];
  /** Exactly what the terminal prints back — captured from a real run, never invented. */
  out?: string[];
  note?: string;
}

/**
 * A terminal block: commands, then the output you should actually see.
 *
 * Showing expected output is the difference between an instruction and a CHECKPOINT. A
 * reader who sees different bytes knows immediately that something is wrong, instead of
 * discovering it three steps later — which is the single most common way a
 * getting-started guide wastes someone's afternoon. Every `out` on this site was captured
 * by running the command (see /tmp/loops/verify_start.py in the engineering log).
 */
export function Terminal({ steps }: { steps: Step[] }) {
  const all = steps.flatMap((s) => s.cmd).join("\n");
  return (
    <div className="my-4 overflow-hidden rounded-xl border border-zinc-800 bg-zinc-950">
      <div className="flex items-center justify-between border-b border-zinc-800 bg-zinc-900/70 px-3 py-1.5">
        <div className="flex items-center gap-1.5">
          <span className="h-2.5 w-2.5 rounded-full bg-zinc-700" />
          <span className="h-2.5 w-2.5 rounded-full bg-zinc-700" />
          <span className="h-2.5 w-2.5 rounded-full bg-zinc-700" />
          <span className="ml-2 font-mono text-[11px] text-zinc-600">terminal</span>
        </div>
        <CopyButton text={all} />
      </div>
      <div className="overflow-x-auto p-4 font-mono text-[13px] leading-6">
        {steps.map((s, i) => (
          <div key={i} className={i > 0 ? "mt-3" : ""}>
            {s.cmd.map((c, j) => (
              <div key={j} className="whitespace-pre">
                <span className="select-none text-emerald-500">$ </span>
                <span className="text-zinc-100">{c}</span>
              </div>
            ))}
            {s.out?.map((o, j) => (
              <div key={j} className="whitespace-pre text-zinc-400">
                {o}
              </div>
            ))}
            {s.note ? (
              <div className="mt-1 font-sans text-[12px] italic text-zinc-600">{s.note}</div>
            ) : null}
          </div>
        ))}
      </div>
    </div>
  );
}

/** A source file with a filename header and a copy button. */
export function FileBlock({
  name,
  children,
  source,
}: {
  name: string;
  children: React.ReactNode;
  source: string;
}) {
  return (
    <div className="my-4 overflow-hidden rounded-xl border border-zinc-800">
      <div className="flex items-center justify-between border-b border-zinc-800 bg-zinc-900/70 px-3 py-1.5">
        <span className="font-mono text-[11px] text-zinc-400">{name}</span>
        <CopyButton text={source} />
      </div>
      {children}
    </div>
  );
}
