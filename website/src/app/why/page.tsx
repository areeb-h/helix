import Link from "next/link";
import { Shell } from "@/components/Shell";
import { Code } from "@/components/Code";

// The argument page. Every defect described here actually happened and is recorded in the
// repository — docs/ROADMAP.md and the commit log — because an argument for a correctness
// mechanism is only worth reading if the mechanism has caught something.

const NAV = [
  { href: "#oracle", label: "The oracle" },
  { href: "#caught", label: "What it caught" },
  { href: "#docs", label: "Docs that run" },
  { href: "#honest", label: "Honest numbers" },
  { href: "#missing", label: "Missing data" },
  { href: "#not", label: "What Helix is not" },
];

export default function WhyPage() {
  return (
    <Shell nav={NAV} navTitle="Why Helix">
      <div className="max-w-3xl">
        <p className="font-mono text-xs uppercase tracking-widest text-emerald-500">
          Why Helix
        </p>
        <h1 className="mt-2 text-4xl font-bold tracking-tight">
          A fast language that is quietly wrong is worse than a slow one
        </h1>
        <p className="mt-5 text-lg leading-relaxed text-zinc-400">
          Scientific code has a failure mode that ordinary software does not: it produces a
          number, the number looks plausible, and nobody finds out. Helix is built around
          one idea aimed squarely at that — make a wrong answer have to be wrong{" "}
          <em>three times</em>, in three independent implementations, before it reaches you.
        </p>

        {/* ------------------------------------------------------------ oracle */}
        <section id="oracle" className="mt-14 scroll-mt-24">
          <h2 className="text-2xl font-semibold tracking-tight">The differential oracle</h2>
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Every Helix program can run three ways: a tree-walking interpreter, a bytecode
            VM, and a Cranelift JIT that compiles numeric kernels to native code. The JIT
            is thousands of lines of code generation standing between your program and its
            answer. So the two simpler engines run beside it, and CI requires all three to
            agree <strong className="text-zinc-200">byte for byte</strong> — on values{" "}
            <em>and on error messages</em>.
          </p>
          <Code
            className="mt-4"
            source={`# the same program, three engines — output must be identical
helix run x.helix                # JIT where eligible
HELIX_NOJIT=1 helix run x.helix  # bytecode VM
HELIX_NOVM=1  helix run x.helix  # tree-walking interpreter`}
          />
          <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
            Agreement alone is not enough, and the tests know it: three engines agree
            trivially if the JIT silently declined to compile. So the suite asserts the
            native kernel actually <em>ran</em> before it trusts a comparison — engagement
            and correctness are separate claims.
          </p>
        </section>

        {/* ------------------------------------------------------------ caught */}
        <section id="caught" className="mt-14 scroll-mt-24">
          <h2 className="text-2xl font-semibold tracking-tight">What it has caught</h2>
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            An argument for a correctness mechanism is worth nothing if the mechanism has
            never found anything. These are real, from this repository&apos;s history —
            each found because one engine answered differently from the others.
          </p>

          <div className="mt-5 space-y-4">
            {[
              {
                t: "An integer subexpression that wrapped in one engine and not another",
                b: "The interpreter computes Int + Int in i64 — which wraps on overflow — and only then promotes to float. A monomorphic f64 kernel does not wrap. So a program with a large integer constant inside a float expression got two different answers, differing by 2⁶⁴.",
                c: `k = 4611686018427387904        # 2^62
ys.map(it + (k + k)).first()
# JIT:    9223372036854775808.0
# VM/tw: -9223372036854775808.0`,
              },
              {
                t: "A comparison that made the same array answer differently",
                b: "min and max on a packed float column compared with IEEE <, where -0.0 and 0.0 tie. The boxed path used a total order, where they do not. The same array gave different answers depending on an internal representation the user cannot see — and min was not even permutation-invariant.",
                c: `[0.0, -0.0].min()          # was  0.0
[0.0, -0.0][0:2].min()     #      -0.0   (same array, sliced)
[0.0, -0.0].sort().first() #      -0.0`,
              },
              {
                t: "A malformed program that failed on two engines and silently succeeded on the third",
                b: "The worst kind: it escaped through `try` into an ordinary boolean, where no error text could reveal it. A malformed call is a mistake in the program, not a condition in the data — but the tree-walker checked the receiver before the call shape, so a `missing` receiver silenced it.",
                c: `(try missing.map()).ok
# tree-walker: true
# VM and JIT:  false`,
              },
            ].map((d) => (
              <div
                key={d.t}
                className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-5"
              >
                <h3 className="font-semibold text-zinc-100">{d.t}</h3>
                <p className="mt-2 text-[14px] leading-relaxed text-zinc-400">{d.b}</p>
                <Code source={d.c} className="mt-3" />
              </div>
            ))}
          </div>
          <p className="mt-5 text-[13px] leading-relaxed text-zinc-500">
            None of these reached a release. All three were found by the oracle, fixed, and
            pinned by a test that fails if they return — and each fix is in the commit log
            with its measurements.
          </p>
        </section>

        {/* ------------------------------------------------------------ docs */}
        <section id="docs" className="mt-14 scroll-mt-24">
          <h2 className="text-2xl font-semibold tracking-tight">Documentation that runs</h2>
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Prose cannot be checked, so prose rots. Helix documents behaviour with{" "}
            <em>executed examples</em>: a <code>##</code> doc comment can contain{" "}
            <code>&gt;&gt;&gt;</code> lines, and CI extracts every one, runs it on all three
            engines, and compares against the output written beneath. A documented example
            that has drifted <strong className="text-zinc-200">fails the build</strong>.
          </p>
          <Code
            className="mt-4"
            source={`## \`%\` and \`//\` are EUCLIDEAN — the remainder is never negative.
##
##     >>> -7 % 3
##     2
##     >>> -7 // 3
##     -3`}
          />
          <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
            This is why the examples on the{" "}
            <Link href="/tour" className="text-emerald-400 underline-offset-2 hover:underline">
              tour
            </Link>{" "}
            carry a verification badge, and the badge means something specific: that exact
            block is extracted by the test gate and its output is required to match.
          </p>
          <p className="mt-4 text-[13px] leading-relaxed text-zinc-500">
            The motivation is a real defect. A comment in this codebase claimed a NaN sorts
            &ldquo;after +inf, as numpy does&rdquo; and named its own example — and that
            example sorts to the <em>front</em>, because the NaN it produces has its sign
            bit set. The comment was wrong about the case it cited, and nothing could catch
            it. An example would have.
          </p>
        </section>

        {/* ------------------------------------------------------------ honest */}
        <section id="honest" className="mt-14 scroll-mt-24">
          <h2 className="text-2xl font-semibold tracking-tight">Numbers you can check</h2>
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Helix loses to single-threaded C on seven of nine comparable kernels. That
            sentence is on the front page of the repository, because a benchmark suite that
            only reports wins is marketing.
          </p>
          <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
            The suite requires every language to print byte-identical output before a
            timing counts, reports <code>%CPU</code> so a wall-clock win bought with 2.8×
            the cores is labelled as such, and keeps its own corrections in the file — a
            previously published figure was measured against a stale binary and was wrong;
            the retraction stayed rather than being quietly replaced.
          </p>
          <Link
            href="/bench"
            className="mt-4 inline-block text-[14px] text-emerald-400 underline-offset-2 hover:underline"
          >
            See the whole record, losses included →
          </Link>
        </section>

        {/* ------------------------------------------------------------ missing */}
        <section id="missing" className="mt-14 scroll-mt-24">
          <h2 className="text-2xl font-semibold tracking-tight">Missing data is not zero</h2>
          <p className="mt-3 text-[15px] leading-relaxed text-zinc-400">
            Real datasets have holes. Most languages make you choose between a value that
            poisons arithmetic silently (<code>NaN</code>) and one that crashes on contact
            (<code>null</code>). Helix has one marker, <code>missing</code>, that propagates
            through arithmetic <em>and reductions</em> — so a sum over a column with a hole
            is <code>missing</code>, not a plausible-looking wrong total.
          </p>
          <Code
            className="mt-4"
            source={`[1, missing, 3].sum()                 # missing — not 4
[1, missing, 3].drop_missing().sum()  # 4 — you said so explicitly
missing ?? 30                         # 30 — a default at the point of use`}
          />
          <p className="mt-4 text-[15px] leading-relaxed text-zinc-400">
            Dropping data is a visible, deliberate step. That is the whole design:
            comparisons are three-valued, so <code>missing == missing</code> is{" "}
            <code>missing</code>, not <code>true</code>.
          </p>
        </section>

        {/* ------------------------------------------------------------ not */}
        <section id="not" className="mt-14 scroll-mt-24 border-t border-zinc-900 pt-10">
          <h2 className="text-2xl font-semibold tracking-tight">What Helix is not</h2>
          <ul className="mt-4 space-y-3 text-[15px] leading-relaxed text-zinc-400">
            <li>
              <strong className="text-zinc-200">Not production-ready.</strong> Version
              0.1.1, one maintainer. The language is stable enough to write real programs
              in; the ecosystem does not exist yet.
            </li>
            <li>
              <strong className="text-zinc-200">Not faster than C.</strong> On most kernels
              it is slower, and the benchmark page says which ones and by how much. It is
              faster than CPython and, on some shapes, than NumPy.
            </li>
            <li>
              <strong className="text-zinc-200">Not a Python replacement.</strong> There is
              no ecosystem, no notebook integration worth the name, no GPU support. It
              embeds a DataFrame engine and can call CPython, and that is the extent of the
              bridge.
            </li>
            <li>
              <strong className="text-zinc-200">Not small.</strong> The binary is ~60 MB
              because it embeds a DataFrame engine. It starts instantly and needs nothing
              installed, which is the trade.
            </li>
          </ul>
          <p className="mt-6 text-[15px] leading-relaxed text-zinc-400">
            If that list did not put you off,{" "}
            <Link href="/start" className="text-emerald-400 underline-offset-2 hover:underline">
              the install takes a minute
            </Link>
            .
          </p>
        </section>
      </div>
    </Shell>
  );
}
