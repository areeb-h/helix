# Full-breadth sweep — findings catalog (2026-07-16)

> **Historical record (2026-07)** — the fixes landed in the 2026-07 sweep commits; regressions pinned in `tests/cli.rs` ("Full-breadth sweep regressions").

Eight-lens adversarial sweep over the subsystems no prior pass audited
(parser/lexer, strings/interpolation, collections, match/try/missing, closures,
typed pipeline, DataFrames/tensors, fuzzer-grammar coverage). ~40 findings, all
empirically verified tri-engine (tree-walker `HELIX_NOVM=1` / VM `HELIX_NOJIT=1`
/ JIT) before triage. Regression pins: `tests/cli.rs` ("Full-breadth sweep
regressions" section).

## Fixed this pass

| # | Severity | What | Fix |
|---|----------|------|-----|
| 1 | critical | VM/JIT closures captured the **outermost** shadowed binding; walker reads the innermost. `fn f(x) = let x = x + 1 in (y => x + y)` → walker 11, VM/JIT 10. Five shapes diverged (let-over-param, match-arm shadow, nested let, same-let-list, interleaved). | `bytecode.rs capturable_env`: dedup duplicate names innermost-wins, matching `resolve_local`'s reverse scan (`resolve_upvalue` takes the first match). |
| 2 | critical | DataFrame column verbs through an **untyped helper parameter** (static Unknown) diverged: `fn g(df) = df.where(@a > 1)` ran on the walker, errored on VM/JIT (`where`/`filter` mis-compiled as comprehension; `sort`/grouped `mean` compiled `@col` as a value). | `bytecode.rs`: `@column` in an argument unambiguously marks a column verb — Unknown receivers with column-mentioning args route to `DfColumnVerb`/`GroupByAgg` (new `mentions_column` scan + `compile_df_verb_guarded`). |
| 3 | major | `missing` fed to a DataFrame verb raised on VM/JIT instead of propagating (ADR-0001): `fn g(df: DataFrame) = df.where(@a > 1); g(missing)` → walker `missing`, VM/JIT "expected a DataFrame, got Missing". | Same helper emits a pure-bytecode `is_missing` guard before every runtime-typed verb op, mirroring the walker's two routes exactly: `where`/`filter` propagate with the predicate untouched; `sort`/`select`/`group`/`with`/aggregations evaluate args first (a `@col` raises the column-reference error), then propagate. |
| 4 | major | Checker froze a `mut` global's definition-time type into fn bodies: `mut d = dataframe(…); fn g() = d.where(it > 1); d = [5,1,2]` → walker `[5,2]`, VM/JIT "expected a DataFrame, got Array". | `types.rs`: track `mut_globals`; inside deferred bodies (fn + lambda) they type as Unknown. Top-level statement flow keeps precise types. |
| 5 | major | `fn` name colliding with an existing global diverged three ways: `fn inf(x)` — walker rejects at definition, VM/JIT register it (exit 0 if uncalled, wrong error if called). Same for user immutable globals (`x = 5; fn x(n)` → VM printed 5). Mut variant: `mut f = 5; fn f(x) = x*2` → walker 6, VM/JIT "not a function". | `bytecode.rs` `Stmt::Func`: immutable-global collision emits the walker's exact raise at the definition point; mutable collision stores the function value into the global (`MakeFunc` + `StoreGlobal`). |
| 6 | major | Parser stack-overflow **abort** (SIGABRT, exit 134, all engines) on deep lambda chains: `x => x => … (2000×)` — lambda bodies were the one `expr()` recursion path skipping the depth counter (ADR-0024 violation). | `parser.rs try_lambda`: both body recursions now `deepen()`/restore like every other structural level → clean "nested or chained too deeply". |

## Fixed in-tree by the parallel stream (uncommitted, verified working)

- NaN sort non-total-order (`methods.rs numeric_cmp` → `total_cmp`) — both orders now `[NaN, 1.0, 2.0, 3.0]`.
- `i64::MIN % -1` / `// -1` process abort → `wrapping_*_euclid` (`ops.rs` + `vm.rs`), tri-engine `0` / `i64::MIN`.
- Walker capture filter (name-based `!globals.contains`) skipping locals that shadow globals — now mutability-aware; shadow probes agree tri-engine.

## ~~⚠ Live divergence INTRODUCED by the uncommitted capture change~~ — RESOLVED

> **RESOLVED (verified 2026-08-11).** The repro below now agrees on all three engines —
> walker, VM and JIT each print `1 / 1`. The `DfJoin` extra-arg divergence noted further
> down is likewise fixed (`tests/corpus/d4_join_arity.expected`, `src/vm.rs`). The section
> is kept because the INVESTIGATION is the useful part; the defect is not live, and this
> page is not a counterexample to the bit-identity claim.

The historical report follows.

`interp.rs:564` snapshots **immutable** globals at closure creation, but `mut n = 100`
legally re-declares an immutable global, and the VM reads globals live:

```helix
n = 1
f = (x => x + n)
print(f(0))   -- 1 everywhere
mut n = 100
print(f(0))   -- walker: 1 (stale snapshot)   VM/JIT: 100
```

Verified live on the current tree. Options: (a) capture only bindings that
*shadow* the global (needs scope-depth in the walker env), or (b) make `mut x`
on an existing immutable global an error (removes the only mutation path,
making the snapshot sound). Must be resolved before that stream lands.

## Live divergences — deferred (fix sites carry uncommitted parallel work)

| What | Repro | Fix site |
|------|-------|----------|
| `Op::DfJoin`'s non-DataFrame arm silently drops extra args: `["a","b"].join("-", "zzz")` → walker arity error, VM/JIT `a-b`. | d4 | `vm.rs:~1057`: non-empty `spec` on the value fallback → raise the walker's arity error. |
| VM error says `filter` where the user wrote `where`: `5.where(it > 1)` → walker "no method \`where\`", VM "no method \`filter\`". | d7 | `vm.rs CompInit` raise: carry the source method name into the op. |
| Match guard on `missing`/non-bool: walker "expected a boolean…" vs VM "`if` condition is missing…" (guards compile to the `if` op). | m2 | Dedicated guard wording shared by `interp.rs` match arm + a flag on `JumpIfFalse` (or new op). |
| Interpolation-hole errors: walker reports the hole expr's AST position, VM the op's `0:0`; both point at wrong lines for runtime errors in holes (holes parsed as line-1 snippets). | i1 | Relocate hole-expr positions at parse time (`parser.rs:1709`) **and** make `vm.rs:861/864` use the hole expr's position. |
| Walker/VM error-order for `x.select(@a)` on a runtime array: walker errors on the arg (column reference), VM on the receiver type. Pathological shapes only; values agree everywhere it matters. | — | Align walker to receiver-first, or give `DfColumnVerb` a walker-identical fallback. |

## Policy decisions needed (consistent behavior today, but engines disagree)

1. ~~**Recursion depth**~~ **RESOLVED (#81, 2026-07-16/17) — aligned, plus the review's catch.** One shared `MAX_CALL_DEPTH = 20_000` (off-by-one corrected: the VM's `frames` includes `<main>`), and the walker gained tail-call optimization (`call_function` trampoline + `eval_tail`) for exactly the shapes the VM's `CallFn`→`TailCallFn` peephole optimizes — gated on the callee's *declared* name, so immutable-global aliases dispatch dynamically like the VM. The adversarial review of that change then exposed something bigger: the walker's flat env gave callees **dynamic scoping** (`fn caller(x) = callee() + 0` let `callee` read the caller's `x` over the global — walker 42, VM 10). Locals now live in a per-frame map swapped at every call boundary; globals in their own map (locals-then-globals, the VM's `LoadGlobal` order). Also fixed: `let` initializer errors leaking installed bindings past `try`, and rebinding a `fn`-declared name (now an error on both engines — the VM's compile-time `CallFn` binding could never honor it). Pinned: `recursion_depth_is_aligned_across_engines`, `walker_scoping_matches_vm_lexically`.
2. ~~**Nested `missing` equality**~~ **RESOLVED (#82, 2026-07-17).** `==`/`!=` are three-valued at any depth via `ops::eq3` (Kleene: definite structural difference wins, otherwise a compared `missing` yields `missing`); set-like ops (`unique`/`frequencies`/`contains`/`index_of`) use total identity equality where `missing` matches `missing`. NaN keeps IEEE semantics in both. ADR-0001 amended; pinned in `three_valued_equality_and_tuple_ordering`.
3. ~~**Duplicate record-literal fields**~~ **RESOLVED (#82).** Parse-time rejection for literals AND patterns ("duplicate field `a` in record literal"); spread-update lists keep last-wins.
4. ~~**Tuple ordering**~~ **RESOLVED (#82).** Lexicographic (first unequal pair decides via the scalar comparison; equal prefix → length), three-valued on `missing`, checker admits Tuple×Tuple.

## Error-message polish (tri-engine consistent, catalogued for one batch)

**STATUS (#84, 2026-07-17): twelve of the fifteen items below are FIXED** (unknown
escapes now error; chained comparisons reject; 0x/`_` literals get targeted
messages; `;` hint; BOM skipped; ASCII hint reworded; builtin-as-value; `it`
hint; mixed-ordering names both types in checker + runtime; sort-missing hint;
`call_label` renders Index/Ident; walker CallValue hint matches the VM; tensor
`round` is elementwise-f64). **Deferred**: the trailing-dot line-glue hint, the
exact-`i64::MIN` literal, and the polars `//`-position threading — grammar/
plumbing changes that deserve their own pass.

- Unknown string escapes silently swallowed: `"\u{0041}"` prints `u41`, `"a\qb"` → `aqb` (lexer `other => other`). Should error with the supported-escape list.
- `1 == 1 == 1` silently prints `false` (`(1==1)==1`); `1 < 2 < 3` at least errors. Reject comparison chaining with a hint.
- `x = 0x10` → "expected end of line … found `x10`" + irrelevant `;` hint; `1_000` splits the same way. Targeted "no hex literals / no `_` separators" errors.
- `;` → bare "unexpected character" with no hint (the parser's perfect `;` hint is unreachable from the lexer).
- UTF-8 BOM → "unexpected character `\u{FEFF}`" (skip it, standard practice).
- Non-ASCII hint claims identifiers are ASCII, but `é`/`π` lex fine (Unicode alphabetic) — reword or restrict.
- `x = 1.` glues the next line via dot-chain continuation → baffling "type Int has no method `print`".
- Call-hint drift: walker "only functions can be called this way…" vs VM "only functions and the built-ins `print`/`dna`/`range`…" (`interp.rs:456` is the odd one out).
- `1 < "a"` says "needs numbers" but `"a" < "b"` is legal; name both operand types.
- `[3, missing, 1].sort()` never names `missing` as the blocker; hint `drop_missing()`.
- `f = print` → "help: assign it first, e.g. `print = ...`" (circular); builtins aren't first-class — say so.
- `it` inside a nested lambda → "did you mean `e`?"; explain the binding rule instead.
- `(fs[0])(1, 2)` arity error labels the callee "`this value`" — render Index/Ident callees in `ast.rs call_label`.
- `-9223372036854775808` silently degrades to Float (magnitude overflows before unary minus applies).
- Polars `//`-in-query rejection reported at 0:0 (`backend/polars.rs:137` hardcodes the position).
- `round(tensor([1e30]))` raises i64-range error though tensors stay Float — round elementwise on the tensor path.

## Coverage gaps (fuzzer grammar + CLI tests)

Ranked by risk-coverage-per-effort (the stale-memo bug hid exactly in gap 1):

1. **Program-level generator for {memoized fn × mut-global read × mutation-between-calls}** — the only continuous protection is 2 hand-written pins. Sketch: `mut g = <lit>` + one of 4 read-indirections + fib-shape fn + rebind + re-call; diff VM vs walker.
2. `and`/`or`/`not` never fuzzed (distinct short-circuit opcodes + three-valued missing) — Bool-producing sub-grammar arm, bias one side to would-error exprs.
3. Strings as first-class fuzz values (comparison, sort, frequencies, indexing) — fixed pool leaf, Bool-producing arms only.
4. Match destructuring patterns (tuple/record/nested) over non-int scrutinees — extend arm 17.
5. Dicts (BTreeMap ordering, DictKey coercion, last-write-wins) — `.to_dict()` + scalar terminal arm.
6. CLI: `emit-hbc` has zero integration tests (blocked on the uncommitted subcommand); capability gates tested for fs only — no `net`/`process`/`env` deny/grant case.

## Verified sound (probed, no findings)

Unicode string semantics (scalar-based, special-casing, ZWJ) · format-spec bounds ·
parse_json recursion caps · dict ordering determinism · record/tuple/dict equality ·
spread semantics · zip truncation · sort stability · match guards/order/bindings ·
try record shape + unwinding · Kleene logic + `??` chains · closure capture-by-value,
3-level chains, upvalue CallValue · Y-combinator · CSV round-trip fidelity ·
join/group edge cases · tensor shape errors · singular det/inv · `**` overflow
crossover · int overflow wrapping parity.
