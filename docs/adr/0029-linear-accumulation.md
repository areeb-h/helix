# ADR 0029 — Amortized-linear accumulation is a language guarantee

- **Status:** **Accepted 2026-08-15; IMPLEMENTED 2026-08-16** — #0 the `(a, a)` guard
  shipped in v0.2.2; #1 the walker's fold (`22368d5`: 262k appends 6,768 → 23 ms, 64k
  dict inserts 17,689 → 16 ms); #2 `Op::AppendStrIntoLocal` + the walker twin
  (`438adfd`: the interpolation fold 4×n → ~1.8× on all three engines, from
  13.6–14.6×). Each landed with its class pin (n-vs-4n ratio, threshold 8×) and its
  semantics table pinned byte-identical across engines and against the pre-change
  released binary. The plan with the verified anchors remains
  [docs/linear-accumulation-plan.md](../linear-accumulation-plan.md).
- **Date:** 2026-08-15
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0024 — Total runtime](0024-total-runtime-no-host-panics.md) (the
  refuse-don't-abort discipline the string cap inherits),
  [ADR 0020 — Reproducibility](0020-dict-ordering-and-reproducibility.md) (why the three
  engines must agree byte-for-byte, which is what makes "optimize one engine" dangerous),
  [ADR 0026 — Library performance boundary](0026-library-performance-boundary.md) (a
  performance cliff is a diagnostic or a fix, never silence).

## Amendment (2026-08-28) — the guarantee stops at a record field

**Reported from the field, reproduced here.** The guarantee holds for a bare
accumulator and does **not** hold when the collection is a field of a record:

| accumulator | 8x the input | class |
|---|--:|---|
| `reduce([], (a, i) => a.concat([i]))` | **3.8x** | linear |
| `reduce({xs: [], k: 0}, (a, i) => {xs: a.xs.concat([i]), k: a.k + 1})` | **22.8x** | quadratic |

*(min of 3, load 0.21, n from 2,000 to 16,000.)*

**Why.** `Op::ConcatIntoLocal` is a take-append-store in ONE instruction, and its own
doc records that this is what makes it safe: the slot is never observably empty, so
there is nothing for a liveness analysis to get wrong. Through a record field there is
no such slot — evaluating `a.xs` clones the `Rc` while the record still holds one, so
`concat` sees a shared array and copies it.

**Why this is not a niche shape.** `mut` is top-level only, so a fold carrying more
than one value *must* carry them in a record — which is what `AGENTS.md` teaches:
*"state that crosses a sequence is threaded, with `reduce`"*. **The recommended idiom
for multi-value accumulation defeats the accumulation guarantee**, with no diagnostic
and nothing in `describe` to warn a reader who is doing exactly what the guide says.

**What changed now.** `helix check --lint` names the shape, with the measured ratios
and what to do instead. ADR 0026 says a performance cliff is a diagnostic or a fix and
never silence; until the fix exists this is the diagnostic. `Array.reduce`'s catalog
entry carries the same boundary, so `helix describe reduce` shows it.

**What a real fix would be, and why neither is in this change.** Two candidates, both
larger than a lint and both needing their own ADR:

1. *Teach the take-append-store through a field.* Recognise
   `acc = {f: acc.f.concat(e), …}` and mutate `acc`'s field in place when both `Rc`s are
   unique, falling back to the copy otherwise — the refcount guard makes the "someone
   else captured it" case safe for free. The hazard is a later initializer in the same
   record literal reading `acc.f` and seeing the appended value; excluding that needs a
   syntactic uniqueness check, and a syntactic check is exactly how an optimization
   grows a cliff at its own edge — the thing ADR 0026 forbids.
2. *A lazy append node in `ArrayData`.* `concat` on a shared array returns
   `Concat { head, tail }` in O(1) and materialises once on first flat read.
   General, no pattern-matching, no cliff, and `ArrayData::Enumerate` is precedent for a
   lazy variant — but it touches every consumer of a core type, where a bug is a wrong
   answer rather than a slow one.

**The three instances found in the field are all latent, and that was checked rather
than assumed**: a server event loop flat from 10 to 320 connections, a training loop
whose per-batch float append is noise against a forward/backward pass, and a template
compiler whose real inputs are 6-8 instructions against an adversarial 800.

## Context

Building a collection one element at a time is how programs are written:

```helix
lines.reduce([], (acc, s) => acc.concat([s]))          # array accumulation
range(0, n).reduce(dict(), (acc, i) => acc.insert(i, i))  # dict accumulation
range(0, n).reduce("", (acc, x) => "{acc}{x}")         # string accumulation
```

Every value in Helix is behind an `Rc`, and a fold's naive evaluation holds two strong
references to the accumulator at the moment the body runs (the binding plus the receiver
value), so every step copies the whole accumulator: **O(n²)**, presenting to the user as
"my program mysteriously hangs" — this project's own history includes a >120 s timeout
whose fix was one spelling away, and the external DX review (#19) hit the same wall three
separate times.

v0.2.0 fixed the array and dict spellings on the VM and JIT with a one-instruction
take-append-store (`Op::ConcatIntoLocal` / `Op::InsertIntoLocal`); `1998db5` extended the
in-place arm to `Values` accumulators. **What remains, measured on the released binary at
load < 0.25, min-of-3, class proven by n-vs-4n ratios:**

| shape | walker | VM | JIT |
|---|---|---|---|
| array fold, 4×n → | **28.8×** (quadratic) | 2.0× (linear) | 1.9× (linear) |
| dict fold, 4×n → | **16.6×** (quadratic) | linear | linear |
| interp fold `"{acc}{x}"`, 4×n → | **13.6×** | **14.3×** | **14.6×** (all quadratic) |

At 256k elements the walker's array fold is 6.64 s against the VM's 0.018 s — a 376×
gap between two engines that must agree byte-for-byte on every *answer*.

**And the design recon found a live wrong answer** on released v0.2.1: duplicate fold
binders `(a, a)` are legal (last-write-wins — the env writes `pa` then `pb`, so `a` is
the ELEMENT), but `emit_reduce_body_and_store` matches the fast path's receiver **by
name** against `pa`, while slot resolution is last-declared-wins:

```helix
print([[1], [2]].reduce([], (a, a) => a.concat([9])))
# walker: [2, 9]      VM: [9, 9]      JIT: [9, 9]      — silent, exit 0
```

The insert twin diverges the same way. Both planned fixes touch this exact function;
building them on the unguarded predicate would propagate the bug into two more sites.

## Decision

**Amortized-linear accumulation for the fold spellings of `concat`, `insert`, and string
interpolation is part of the language contract, on all three engines** — with one
uniform soundness mechanism and one uniform escape hatch:

1. **Rc uniqueness is the only aliasing oracle.** No syntactic aliasing analysis, ever.
   The transformation is always *take-append-store*: evaluate the argument first (it
   sees the live accumulator), validate everything that can fail, then take the
   accumulator out of its slot/binding (`mem::replace` with a placeholder) so a
   non-aliased value becomes unique, and route through `Value::concat_in_place` /
   `insert_in_place` / the string twin — whose `Rc::get_mut` check **is** the safety
   proof. Anything still shared (a closure captured the accumulator, `acc.concat([acc])`,
   a shared init, scan's snapshots) fails `get_mut` and falls to the copy path:
   **correct results at the old cost, never a wrong answer at any cost.**

2. **The recognizer is deliberately narrow and identical everywhere.** The fast path
   fires only on the exact body shapes `acc.concat(e)`, `acc.insert(k, v)`, and
   `"{acc}…tail…"` — receiver/first-hole the bare accumulator binder, no named
   arguments, no format spec on the accumulator hole, no later hole mentioning the
   accumulator, **and `pa != pb`** (the duplicate-binder guard the live bug demands).
   Everything else keeps the general path unchanged. A whole-body take is forbidden:
   real programs (`(a, x) => if x > a then x else a`) mention the accumulator twice, and
   a broad take would read a placeholder on the second mention.

3. **Errors and representation are pinned, not approximated.** The take happens only
   after every fallible step: error texts stay word-for-word the general path's, a
   mid-fold error leaves the slot/binding untouched (the `try`-catch contract, proven
   identical on all three engines today), and the string append obeys the existing
   `MAX_STRING_LEN` cap with its byte-identical message *and* its current span
   attribution. Where representation is semantic (`array_sniff` repacking), the in-place
   arm exists only where the result representation is provably identical — the
   established `concat_in_place` doctrine.

4. **What deliberately stays slow:** `scan` (its snapshots are co-owners; in-place
   mutation would corrupt history — it stays on the copy path by the same `get_mut`
   argument, and its output is pinned), and every declined shape above. Slow-but-correct
   is acceptable; wrong is not. If a declined shape matters in practice, the fix is a
   diagnostic or a widened recognizer with its own proof — never a weaker oracle.

## Consequences

- The tree-walker stops being the odd engine out: its fold gains the same recognizer and
  the same primitives the VM ops use, transplanted — not a parallel invention. Its
  binder save/restore choreography (including the hand-written `(a, a)` restore case) is
  untouched.
- String interpolation folds gain `Op::AppendStrIntoLocal` on the VM/JIT and the
  walker-side twin, making the *third* accumulation spelling linear — with growth made
  fallible (`try_reserve`) so a near-cap append refuses rather than aborts (ADR 0024).
- The `(a, a)` guard lands first, as its own fail-first-pinned bug fix.
- The complexity guarantee gets n-vs-4n class pins in the gate (ratio assertions, not
  wall-clock), so a future regression to quadratic is a test failure, not a user report.
- Cost accepted: the recognizer's narrowness means users can still write quadratic folds
  (`acc` twice, spec on the acc hole). ADR 0026's principle applies — those are
  candidates for a `helix check` note, not for silent cleverness.
