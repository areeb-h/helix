# ADR 0026 — Is library code meant to be fast? The indirect-call boundary

- **Status:** **Accepted 2026-08-13 — Helix libraries are first-class; option (b),
  monomorphize at the call site.** Scheduled, not implemented. The measurements are
  reproducible with `target/release` and the programs quoted below.
- **Date:** 2026-08-13
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0007 — Tensor backend](0007-tensor-backend.md) and
  [ADR 0012 — DataFrame backend seam](0012-dataframe-backend-seam.md) (both delegate the
  serious data types to Rust crates, which is the architecture this question is really
  about), [ADR 0017 — Methods and functions](0017-methods-and-functions.md).

## Context

A foundation audit concluded that **"the JIT is invisible to library code"**. That
conclusion was wrong, and finding out why produced two fixes worth 43× and 36×
(`becf927`, `42a972b`). It is worth stating plainly what is now true before asking what is
left, because the question the audit was really pointing at survives its own bad answer.

**Measured 2026-08-13, `target/release`, n=5M, min of 3, every run's output asserted.**
Four spellings of one integral, `Σ f(i)` with `fn f(x) = x * x * 0.5 + x`. The ONLY
difference between them is how the integrand is reached:

| spelling | JIT | `HELIX_NOJIT=1` | gain | vs inlined |
|---|---|---|---|---|
| inlined, no call at all | 0.008s | 0.691s | 81.8× | 1.0× |
| called BY NAME from the reduce body | 0.017s | 0.571s | 33.3× | **2.0×** |
| passed as a VALUE — `integrate(f, n)` | 0.606s | 0.604s | **1.0×** | **71.7×** |
| the same, via `map(…).sum()` | 0.620s | 0.609s | **1.0×** | 73.4× |

(The first three agree to the last bit. The fourth differs in the low digits because
`map().sum()` sums pairwise where `reduce` sums left-to-right — float addition is not
associative. That is ADR-0001 territory, not a defect.)

**The 1.0× gain is the whole finding.** A function passed as a value does not make the
kernel slower; it makes the kernel *not exist*. The JIT cannot name the callee, so it cannot
monomorphize it, so the entire enclosing loop falls back to the bytecode VM. Naming your
callee costs 2×. Passing it as a value costs **72×**.

So the boundary is no longer "library code is slow". It is exactly this:

- A library that exposes **named entry points** — `stats.mean(xs)`, `bio.gc_content(s)`,
  anything the caller invokes directly — now compiles natively, including across a module
  boundary, and an unannotated parameter is within 8% of an annotated one.
- A library whose interface **is a callback** — `integrate(f, a, b)`, `solve(residual, x0)`,
  `optimize(objective)`, a strategy or policy argument — runs on the VM, always.

That second list is not a corner. It is most of what a numerics library's interface looks
like, and it is the shape Helix's own `map`/`filter`/`reduce` are built from — the
difference being that those are *builtins*, so the compiler special-cases them.

## The question

**Is a Helix library meant to be as fast as a Helix builtin, or is the architecture
"serious types and hot kernels are Rust; Helix is the layer above"?**

No ADR says. ADR-0007 (tensors → faer) and ADR-0012 (DataFrames → Polars) both *imply* the
second answer without ever generalizing it, and the implication has never been tested
against the case where the thing being delegated is not a data type but a *control-flow
shape*.

The answer decides real work in both directions, which is why it is worth an ADR rather
than a default:

- **If "the layer above" is the answer**, the 72× is a documented boundary, not a bug. It
  gets written into the library-author guidance ("expose named functions; a callback
  parameter runs interpreted"), native indirect calls are declined *permanently*, and
  several open ROADMAP items close as won't-do.
- **If Helix libraries are meant to be first-class**, this is the largest single piece of
  work in the project, and it should be scheduled as such rather than discovered one
  benchmark at a time.

## Options

**(a) Accept the boundary. Document it, decline native indirect calls forever.**
Cheapest, and consistent with how the tensor and DataFrame decisions already went. The cost
is that "write your hot loop as a higher-order function" — the ordinary way to factor
numerical code — is a 72× trap with no diagnostic, and Helix's stated aim of not having
performance cliffs the user cannot see gets an asterisk.

**(b) Monomorphize at the call site.** When `integrate(f, n)` is called with a statically
known callee, specialize `integrate` for that callee — the same thing the JIT already does
per `NumKind`. Degrades to the VM when the callee is genuinely dynamic, so nothing gets
slower. Costs code growth and a specialization cache, and needs a policy for how many
specializations one function may have.

**(c) Native indirect calls.** Emit a call through a function pointer with a runtime guard
that the value is the specialization the kernel was compiled against. Most general, most
expensive, and it needs a uniform ABI across every specialization — the mixed ABI's
trailing poison pointer already shows how fiddly that surface is.

## Decision: libraries are first-class. Option (b), monomorphize at the call site

Accepted 2026-08-13, against the stated goal of *a language people build packages and
libraries on*. The goal answers the question directly: **you cannot ask people to build
libraries on a language where the standard way to factor a numerical routine is a 72×
trap.** "Serious types are Rust, Helix is the layer above" is a coherent architecture for a
*scripting* language over Polars and faer — but it is not one anybody writes a package
ecosystem for, because every performance-critical library would have to be written in Rust,
and then Helix is a configuration language for other people's crates.

So (a) is rejected, and rejected explicitly rather than by default — which matters, because
(a) is what the project has been doing implicitly by never deciding.

**(b) over (c).** Helix's JIT is already a monomorphizing compiler — `mixed_fn_sigs`,
`int_eligible_fns` and the map/reduce kernels all specialize by kind — so call-site
specialization extends the existing design instead of introducing a second one, and its
failure mode is the VM fallback the codebase already handles everywhere. (c), a guarded
indirect call, is the general answer to a question (b) answers for the call sites that
actually occur; the mixed ABI's trailing poison pointer is a fair warning about how fiddly
a uniform cross-specialization ABI gets. (c) stays on the table only if a real program is
found whose callee is genuinely dynamic *and* hot.

**This does not jump the queue.** It is scheduled AFTER last-use liveness (the append
wall), which blocks a strictly larger class of library: a callback-shaped API is slow,
whereas a library that grows a collection — tokenizer, parser, index builder, symbol table
— is not writable at all. Fixing "slow" before "impossible" would be the wrong order.

**Until it lands, the boundary is a documented diagnostic, not silence.** The current state
— where the only way to discover which of your APIs is 72× slower is to measure it — is the
part that is indefensible regardless of which option was chosen, so it is fixed first and
independently: calling a function that arrived through a parameter, from inside a
comprehension, should say so the way the JIT already explains its other declines.

## What is NOT being asked

This is not about `map`/`filter`/`reduce` taking a lambda. Those are builtins the compiler
lowers directly, and a lambda written *at* the call site is compiled as part of the kernel —
that is the 0.008s row. The question is only about a function reaching a loop **through a
parameter**.

## Consequences

- Whichever way it goes, `docs/adoption.md` and the library-authoring guidance need a
  paragraph stating it outright. The current silence is the actual defect: a library author
  today cannot find out which of their APIs is 72× slower except by measuring.
- If (a): several ROADMAP entries about indirect calls close as won't-do, and the boundary
  should be a *diagnostic*, not just prose — a note when a callback parameter is called
  inside a comprehension, the way the JIT already explains other declines.
- If (b) or (c): it is scheduled work, and it comes after last-use liveness (the append
  wall), which blocks a larger class of library than this does.

## Open questions

- Does the answer differ for a callback that is called **once** (a comparator handed to
  `sort`, run n log n times inside a builtin) versus one called **per element inside user
  code**? The first is already native; only the second pays the 72×.
- If (b), what is the specialization budget per function, and what happens when it is
  exhausted — silently fall back, or warn?
