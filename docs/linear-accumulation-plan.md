# Linear accumulation — the implementation plan

> **STATUS 2026-08-16: all three landed.** #0 in v0.2.2 (`d693a4b`), #1 at `22368d5`
> (walker 262k appends 6,768 → 23 ms, dict 64k 17,689 → 16 ms), #2 at `438adfd`
> (interp fold ~1.8× per 4×n on all engines). One deviation from the spec as written:
> #2's runtime restores the slot and reports on a refused reservation rather than
> erroring after the take — strictly stronger than the drafted behavior. The
> AGENTS.md footgun list and `docs/dx-plan.md`'s string items can now drop their
> quadratic-fold warnings.

Implements [ADR 0029](adr/0029-linear-accumulation.md). Every anchor below was verified
against the live tree at `b23c314` by a three-agent recon (2026-08-15); every measurement
is from the released v0.2.1 PGO binary at load < 0.25, min-of-3, with the complexity
class proven by n-vs-4n ratios — never wall-clock alone, and **never a gate build
compared against the PGO release** (the proven 1.9× phantom from the affine work).

Work order is dependency order: #0 is a live wrong answer inside the function #1 and #2
both extend.

---

## #0 — the `(a, a)` guard: a live three-engine divergence (land first, alone)

**The bug, on released v0.2.1 (silent, exit 0):**

```helix
print([[1], [2]].reduce([], (a, a) => a.concat([9])))
# walker: [2, 9]     VM/JIT: [9, 9]
# insert twin: [d1, d2].reduce(dict(), (a, a) => a.insert("n", a.count())) diverges too
```

**Mechanism:** duplicate binders are legal, last-write-wins — the walker writes `pa`
then `pb` (interp/comprehensions.rs:318-319), and the compiler's `resolve_local` is
last-declared-wins (bytecode.rs:182-202) — so `a` in the body is the ELEMENT. But
`emit_reduce_body_and_store` (bytecode/comprehensions.rs:1388-1415) matches the fast
path's receiver **by name** (`n == pa`, :1399) and emits `ConcatIntoLocal(acc_slot)`,
folding into the ACCUMULATOR on VM/JIT while the walker folds the element.

**Fix:** add `pa != pb` to the guard at :1397-1399. Zero performance cost (declines a
pathological spelling to the general path, which is then identical on all engines).
The identical decline already exists at the two sibling sites :855 and :1186 — this is
the third copy of a known rule, not a new idea.

**Pins (fail-first on the current binary):** the concat and insert shapes above, all
three engines, with Array/Dict element types (a scalar element is masked by the
checker). Also pin the benign `(a, a)` scalar fold (`reduce(0, (a, a) => a + a)` → `4`)
so the decline never widens.

## #1 — the walker's fold: transplant the VM's discipline (the 6.6 s → linear fix)

**Why the walker is quadratic** (all verified, quotes in the recon): the reduce arm
MOVES the accumulator into the env each element (`self.env.get_mut(pa).unwrap().value =
acc`, interp/comprehensions.rs:318) — at that moment the env is the sole owner — but
evaluating the body clones it back out (`b.value.clone()`, interp.rs:412-413), so
`concat` always sees Rc count ≥ 2. `concat_in_place`/`insert_in_place` have exactly two
callers, both in vm.rs — the walker never calls them; its packed concat arm copies the
whole receiver per call (its own comment says so, methods.rs:336-340) and its dict
`insert` clones the whole map unconditionally (methods.rs:625-631).

**The fix — recognize once, take per element, reuse the primitives:**

1. Before the loop (near the binder extraction, :270-293), run the SAME recognizer as
   the guarded `emit_reduce_body_and_store`: body is exactly
   `Expr::Method { recv: Ident == pa, name: "concat"|"insert", args (1|2), named empty }`
   **and `pa != pb`** (#0's guard) **and the verb is `reduce`, never `scan`** (scan's
   `out.push(acc.clone())` at :324 keeps a snapshot owner alive; it must stay on the
   copy path — pinned by interp/tests.rs:2084).
2. Per element, for the recognized shape only: evaluate the argument(s) FIRST with the
   binding intact (so `acc.concat([acc.count()])`-style arguments still see the live
   value — they simply keep the Rc shared and decline in-place at runtime); validate
   with the walker's existing error constructors (error text parity is pinned at
   tests/cli.rs:3470-3487); then `std::mem::replace` the binding's value with
   `Value::Unit` and route through `Value::concat_in_place` / `insert_in_place` —
   **never a hand-rolled extend**. Store the result as the new `acc`.
3. Touch nothing else: the save/restore choreography (:302-307, :334-350, including the
   hand-written `(a, a)` restore at :339-350) and the error arm (:327-331) stay as they
   are. On the fast path an argument error fires before the take, so the binding is
   intact for the restore — the same validate-before-take ordering as vm.rs:682-705.

**Also in this change (same file, same shape of bug):** the reduce arm iterates
`items.to_values().iter()` (:317) — the packed-array boxing hazard eval_pattern_loop
already fixed with `iter_values()` (:395). Switch it; it is the same ADR-0024 lesson.

**Soundness is inherited, not proven anew:** shared init (`reduce(pre, …)` — `pre`
intact after, proven on all engines), closure-captured acc (captured BY VALUE at
interp.rs:713 → extra owner → get_mut declines), `acc` in the argument, and `(a, a)`
(declined by #0's guard) all fall to the copy path with identical answers.

**Pins to add (none of these exist today — the recon proved the automated oracles are
blind here: vmparity is 2-engine with zero fold-append shapes, stranger.py never
generates the shape):**
- error-mid-fold restore, all three engines: `r.ok == false`, `r.value == missing`, the
  init-source variable intact, a shadowed outer binding restored (the p4/p5 probes —
  currently pinned NOWHERE).
- walker linearity class pin: n-vs-4n ratio on the array fold, walker engine, threshold
  ~6× (linear is ~4×, quadratic is ≥16×), pattern copied from tests/cli.rs:3408.
- the existing 3-engine fold suites (cli.rs:3413-3488 arrays incl. both error wordings,
  :3493-3555 dicts, :4854-4873 string/repr) are the pre-existing net — run green.

**Expected result:** walker 256k array appends 6.64 s → the VM's class (~linear);
dict fold likewise; walker string-array fold inherits `1998db5`'s Values arm through
`concat_in_place` automatically.

## #2 — `Op::AppendStrIntoLocal`: the interp fold, quadratic on ALL engines

**The shape and the class:** `range(0, n).reduce("", (acc, x) => "{acc}{x}")` — 13.6–
14.6× per 4×n on walker/VM/JIT alike (0.89 s at 256k chars; minutes at genome scale).
Mechanism: `Op::Interp` builds a fresh `String` and copies the whole accumulator into it
every element (vm.rs:1300, :1315 → value.rs:901); no kernel admits a Str init
(comprehensions.rs:691-741).

**Op design** (mirror `ConcatIntoLocal`'s four-paragraph contract, ops.rs:162-183):
`AppendStrIntoLocal(u32, Rc<Vec<InterpPart>>)` — the slot plus the same parts vector
`Op::Interp` carries, with parts[0] known-by-construction to be the bare acc hole.

**Emission — one new arm in `emit_reduce_body_and_store`** (:1388-1415; both reduce
call sites :799 and :1100 inherit it). Guard: body is `Expr::Interp(parts)` AND
`parts[0]` is `InterpPart::Expr(Ident == pa, None)` (no format spec — a spec re-pads
the WHOLE accumulator each iteration, semantics not append: the `"{acc:>4}{x}"` probe
prints `    012`) AND `pa != pb` AND **no later hole mentions `pa`** (an acc-mention
pushes an Rc clone → the take always declines → silently quadratic; decline at compile
time instead, the deliberately-narrow house precedent). Compile parts[1..] hole exprs
left-to-right (same order as bytecode.rs:1400-1405, skipping only the acc LoadLocal,
which is side-effect-free), then emit.

**Runtime (the one exhaustive `match op`, vm.rs:668), in this exact order:**
1. Render parts[1..] into a scratch `String` from the stacked hole values, reusing
   `write_value`/`fs.apply` with per-hole `e.position()` errors exactly as
   vm.rs:1302-1318 — **all fallible work before the take**, so any error leaves the
   slot untouched (the try-catch pin).
2. Cap check `acc.len() + scratch.len() > MAX_STRING_LEN` with the byte-identical text
   AND help line of interp.rs:400-407 / vm.rs:1322-1331 (`interpolated string exceeds
   1073741824 bytes` + the incremental-build hint) — including the current line-1:0
   span attribution, which is odd but identical on all three engines today; changing it
   on one engine is an oracle break. Monotone-equivalent to the per-part check.
3. Slot not a `Str` (init = 0 — the general path FORMATS it: `0012` probe, all
   engines): fall back to fresh-build `write_value(acc) + scratch`, store. Never a new
   error. Iteration 2 re-engages the fast path.
4. Otherwise `mem::replace` the slot (vm.rs:709 pattern), `Rc::get_mut` →
   **`try_reserve` + `push_str`** in place when unique (fallible growth: a near-cap
   doubling must refuse, not abort — the B3 lesson), else one content clone + append
   (the copy-fallback discipline). Shared init (`"seed"` probe) stays correct by the
   same Rc argument.

**Blast-radius facts (verified, correcting the earlier dx-plan):**
- `scan` does NOT share the emit path — it emits its body inline (:1256-1260). Do not
  unify; pin scan's string-fold output `["0","01","012","0123"]` on all three engines
  instead (a future unification would corrupt snapshots silently).
- hbc.rs needs NO mandatory row — the wildcard at :397/:587 auto-rejects a new op from
  the .hbc subset; the tailored row beside :582 is optional polish.
- The only exhaustive Op match is vm.rs:668 — rustc forces exactly one extension.
- The unwrap-budget ratchet (tests/cli.rs:3926 pins src/vm.rs at 60 panicking calls)
  trips on the op's compiler-guaranteed pops: bump with site-comment justification in
  the same commit (precedent :3923-3925).

**Pins:** value parity for the fold vs the general path (mixed shapes, spec-on-element
holes, non-Str init, shared init); an ACTIVE fold-shaped cap test using the
one-doubling-past-the-cap trick (vm/tests.rs:5918-5920 — the existing cap test is
`#[ignore]`d and 2-engine, so nothing in the gate catches drift today); the n-vs-4n
class pin on all three engines; scan's snapshot output.

## Acceptance

Each numbered item lands as its own commit, gate green, with its pins in the same
commit, each behavioral pin confirmed to fail on the pre-change binary first. After #2,
re-measure the ADR's table on an idle box and update it in place — the walker column
and the interp row must all read ~4× per 4×n. Then the ADR's status line flips to
implemented, and `docs/dx-plan.md`'s string items (FIX 2/FIX 3) get pointed here.
