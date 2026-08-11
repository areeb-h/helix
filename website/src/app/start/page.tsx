import Link from "next/link";
import { Shell } from "@/components/Shell";
import { Code } from "@/components/Code";
import { Terminal, FileBlock } from "@/components/Terminal";

// EVERY command and every line of output on this page was captured from a real run
// (verify_start.py). Nothing is paraphrased from `helix help`. If a step here does not
// reproduce, that is a bug in Helix or in this page — not in the reader.

const NAV = [
  { href: "#install", label: "1 · Install" },
  { href: "#hello", label: "2 · Hello, Helix" },
  { href: "#real", label: "3 · Something real" },
  { href: "#repl", label: "4 · The REPL" },
  { href: "#check", label: "5 · Check it" },
  { href: "#build", label: "6 · Ship a binary" },
  { href: "#project", label: "7 · Projects & tests" },
  { href: "#next", label: "Where next" },
];

const HELLO = `print("Hello, Helix!")`;

const REAL = `# reads.helix — quality-filter some sequencing reads, then summarize.
reads = [
    {id: "r1", seq: dna("ATGCGC"), q: 38},
    {id: "r2", seq: dna("ATTTTA"), q: 12},
    {id: "r3", seq: dna("GCGCGC"), q: 41},
]

kept = reads.where(r => r.q >= 30)

print("kept {kept.count()} of {reads.count()} reads")
print("mean quality: {kept.map(r => r.q).mean()}")
print("GC content:   {kept.map(r => r.seq.gc_content()).mean()}")`;

export default function StartPage() {
  return (
    <Shell nav={NAV} navTitle="Get started">
      <div className="max-w-3xl">
        <p className="font-mono text-xs uppercase tracking-widest text-emerald-500">
          Get started
        </p>
        <h1 className="mt-2 text-4xl font-bold tracking-tight">
          From nothing to a shipped binary
        </h1>
        <p className="mt-4 text-lg leading-relaxed text-zinc-400">
          Seven steps, about ten minutes. Every command below shows{" "}
          <span className="text-zinc-200">the output you should actually see</span> — so if
          your terminal disagrees, you know at that step rather than three steps later.
        </p>

        {/* ---------------------------------------------------------------- 1 */}
        <section id="install" className="mt-14 scroll-mt-24">
          <StepHead n={1} title="Install" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Helix is a single self-contained binary — no runtime, no Python, no system
            BLAS. It is around 60 MB because it embeds a DataFrame engine, and it starts
            instantly.
          </p>

          <p className="mt-5 text-[15px] leading-relaxed text-zinc-400">
            <strong className="text-zinc-200">macOS and Linux.</strong> Downloads the
            binary for your platform, checks its SHA-256, and puts it on your PATH:
          </p>
          <Terminal
            steps={[
              {
                cmd: [
                  "curl -LsSf https://raw.githubusercontent.com/areeb-h/helix/main/install.sh | sh",
                ],
                note: "It prints each step: which artifact it fetched, the checksum result, and where it installed. A checksum MISMATCH aborts rather than warning. HELIX_MUSL=1 fetches the fully-static build; HELIX_INSTALL_DIR changes the location.",
              },
            ]}
          />
          <p className="text-[13px] leading-relaxed text-zinc-500">
            <strong className="text-amber-400/90">This does not work yet</strong>, and every
            other output on this page was captured from a real run, so there is no
            transcript here to show. <code>v0.1.0</code>&rsquo;s release pipeline published
            four of six platforms and no <code>SHA256SUMS</code>; the installer refuses to
            install what it cannot verify, so it correctly declines. The three causes are
            fixed and the next tag produces the first installable release — at which point
            a real transcript replaces this paragraph. Until then,{" "}
            <code className="text-zinc-400">cargo install --path .</code> below is the
            working path.
          </p>

          <p className="mt-5 text-[15px] leading-relaxed text-zinc-400">
            <strong className="text-zinc-200">Windows.</strong> PowerShell, no admin
            rights — it installs to your user profile and updates your user PATH:
          </p>
          <Terminal
            steps={[
              {
                cmd: [
                  "irm https://raw.githubusercontent.com/areeb-h/helix/main/install.ps1 | iex",
                ],
                note: "Verifies with Get-FileHash, installs to %LOCALAPPDATA%\\Helix\\bin, and adds it to your user PATH — no admin prompt. Open a new terminal afterwards for PATH to take effect.",
              },
            ]}
          />

          <div className="mt-5 rounded-xl border border-zinc-800 bg-zinc-900/40 p-4">
            <div className="text-sm font-semibold text-zinc-100">
              Or build it yourself
            </div>
            <p className="mt-1.5 text-[13px] leading-relaxed text-zinc-400">
              With{" "}
              <a
                href="https://rustup.rs"
                className="text-emerald-400 underline-offset-2 hover:underline"
              >
                Rust
              </a>{" "}
              installed, from a checkout:{" "}
              <code className="text-zinc-300">cargo install --path .</code>. Takes a few
              minutes — it compiles the JIT and the DataFrame engine. The installers fall
              back to exactly this if no prebuilt binary matches your platform.
            </p>
          </div>
        </section>

        {/* ---------------------------------------------------------------- 2 */}
        <section id="hello" className="mt-14 scroll-mt-24">
          <StepHead n={2} title="Hello, Helix" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            One file, one line. No <code>main</code>, no imports, no boilerplate.
          </p>
          <FileBlock name="hello.helix" source={HELLO}>
            <Code source={HELLO} className="rounded-none border-0" />
          </FileBlock>
          <Terminal
            steps={[
              { cmd: ["helix run hello"], out: ["Hello, Helix!"] },
              {
                cmd: ["helix hello"],
                out: ["Hello, Helix!"],
                note: "Both the `.helix` extension and the `run` are optional — `helix run hello.helix` works too, and an exact file always wins over an appended extension.",
              },
              {
                cmd: ['helix eval "print([1, 2, 3].sum())"'],
                out: ["6"],
                note: "…and for a one-liner you do not need a file at all.",
              },
            ]}
          />
        </section>

        {/* ---------------------------------------------------------------- 3 */}
        <section id="real" className="mt-14 scroll-mt-24">
          <StepHead n={3} title="Something worth writing" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Hello-world tells you nothing about a language. This is closer to the point:
            records, a filter, string interpolation, and DNA as a first-class type — no
            imports, because these are the language.
          </p>
          <FileBlock name="reads.helix" source={REAL}>
            <Code source={REAL} className="rounded-none border-0" />
          </FileBlock>
          <Terminal
            steps={[
              {
                cmd: ["helix run reads.helix"],
                out: [
                  "kept 2 of 3 reads",
                  "mean quality: 39.5",
                  "GC content:   0.8333333333333333",
                ],
              },
            ]}
          />
          <p className="text-[13px] leading-relaxed text-zinc-500">
            That GC number is not rounded for the brochure. Helix prints the{" "}
            <code className="text-zinc-400">f64</code> it computed, and this site prints
            what Helix printed.
          </p>
        </section>

        {/* ---------------------------------------------------------------- 4 */}
        <section id="repl" className="mt-14 scroll-mt-24">
          <StepHead n={4} title="Poke at it" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            The REPL evaluates an expression per line. Ctrl-D exits.
          </p>
          <Terminal
            steps={[
              {
                cmd: ["helix repl"],
                out: [
                  "Helix 0.1.0 — interactive session. Type an expression and press Enter; Ctrl-D to exit.",
                  "helix> 1 + 1",
                  "2",
                  "helix> [3, 1, 2].sort()",
                  "[1, 2, 3]",
                  "helix> ",
                ],
              },
            ]}
          />
        </section>

        {/* ---------------------------------------------------------------- 5 */}
        <section id="check" className="mt-14 scroll-mt-24">
          <StepHead n={5} title="Check before you run" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            <code>helix check</code> answers &ldquo;does this still compile?&rdquo; without
            running a line of it — the same front end <code>helix run</code> uses, stopped
            just before execution. Give it as many paths as you like; the whole of this
            repository&rsquo;s 85 programs check in about 30&nbsp;ms.
          </p>
          <Terminal
            steps={[
              { cmd: ["helix check hello"], out: ["ok   hello.helix"] },
              {
                cmd: ["helix check hello.helix report.helix"],
                out: [
                  "ok   hello.helix",
                  "FAIL report.helix",
                  "error: `totl` is not defined",
                  "  --> report.helix:3:7",
                  "  |",
                  "3 | print(totl)",
                  "  |       ^",
                  "help: assign it first, e.g. `totl = ...`.",
                  "checked 2 files, 1 failed",
                ],
              },
            ]}
          />
          <p className="text-[13px] leading-relaxed text-zinc-500">
            It is a <em>type</em> check, not a promise: a program that checks clean can
            still fail at run time. And it never executes anything, so it is safe to point
            at code you have not read.
          </p>
        </section>

        {/* ---------------------------------------------------------------- 6 */}
        <section id="build" className="mt-14 scroll-mt-24">
          <StepHead n={6} title="Ship a standalone binary" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            <code>helix build</code> bundles a script into an executable that runs on a
            machine with no Helix, no toolchain, and no runtime installed.
          </p>
          <Terminal
            steps={[
              {
                cmd: ["helix build hello"],
                out: ["built standalone executable: hello"],
              },
              { cmd: ["./hello"], out: ["Hello, Helix!"] },
            ]}
          />
          <p className="text-[13px] leading-relaxed text-zinc-500">
            The executable is large — it embeds the whole engine, JIT included — which is
            the trade for having nothing to install on the far end. The reasoning, and what
            has been tried to shrink it, is written up in{" "}
            <Link
              href="/docs"
              className="text-emerald-400 underline-offset-2 hover:underline"
            >
              the docs
            </Link>
            .
          </p>
        </section>

        {/* ---------------------------------------------------------------- 7 */}
        <section id="project" className="mt-14 scroll-mt-24">
          <StepHead n={7} title="Projects and tests" />
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            When one file stops being enough: <code>helix new</code> writes a manifest, and{" "}
            <code>helix test</code> runs every <code>*_test.helix</code> beside your code.
          </p>
          <Terminal
            steps={[
              {
                cmd: ["helix new demo"],
                out: ["Created helix.toml for package `demo`."],
              },
              {
                cmd: ["helix test"],
                out: [
                  "running 1 test file",
                  "  ok    math_test.helix",
                  "",
                  "1 passed",
                ],
              },
            ]}
          />
          <p className="text-[13px] leading-relaxed text-zinc-500">
            Dependencies come from a path or a tarball —{" "}
            <code className="text-zinc-400">helix add name --path ../lib</code> — and{" "}
            <code className="text-zinc-400">helix sync</code> writes a lockfile.
          </p>
        </section>

        {/* --------------------------------------------------------------- next */}
        <section id="next" className="mt-16 scroll-mt-24 border-t border-zinc-900 pt-10">
          <h2 className="text-2xl font-semibold tracking-tight">Where next</h2>
          <div className="mt-5 grid gap-3 sm:grid-cols-3">
            {[
              {
                href: "/tour",
                title: "The tour",
                body: "The language in 20 runnable files — every example executed on all three engines by CI.",
              },
              {
                href: "/reference",
                title: "Reference",
                body: "Every builtin and method, generated from the binary itself with `helix describe`.",
              },
              {
                href: "/docs",
                title: "The docs",
                body: "How the engines agree, what integer overflow does, where it is still slow.",
              },
            ].map((c) => (
              <Link
                key={c.href}
                href={c.href}
                className="group rounded-xl border border-zinc-800 bg-zinc-900/40 p-4 hover:border-emerald-800 hover:bg-zinc-900/70"
              >
                <div className="font-semibold text-zinc-100 group-hover:text-emerald-300">
                  {c.title}
                </div>
                <p className="mt-1.5 text-[13px] leading-relaxed text-zinc-500">{c.body}</p>
              </Link>
            ))}
          </div>
          <p className="mt-6 text-[13px] leading-relaxed text-zinc-500">
            Stuck? <code className="text-zinc-400">helix help</code> lists every command,
            and <code className="text-zinc-400">helix doc Array</code> lists every method
            on a type — offline, from the binary you already have.
          </p>
        </section>
      </div>
    </Shell>
  );
}

function StepHead({ n, title }: { n: number; title: string }) {
  return (
    <div className="flex items-center gap-3">
      <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full border border-emerald-800 bg-emerald-950/60 font-mono text-[12px] text-emerald-400">
        {n}
      </span>
      <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
    </div>
  );
}
