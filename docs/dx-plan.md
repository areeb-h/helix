# DX hardening plan — from the #19 review, every claim verified

Produced 2026-08-15 by an 8-agent audit of the #19 recommendations against the
LIVE v0.2.1 binary and repo. Each claim was reproduced or refuted with pasted
output before anything was drafted; the review was measured against an older
binary in places, and the stale items are recorded as such so nobody re-fixes
them. Ordering is leverage/risk as argued per item.

## Stale — already fixed, or wrong as written (with proof)

- B2 — 'grouped aggregations silently drop missing': FIXED in v0.2.1. `dataframe({g:["a","a","a"],v:[1.0,3.0,missing]}).group(@g).mean(@v)` prints `null`, rc=0, on the installed release binary.
- C1 — 'DNA IUPAC arithmetic is wrong': FIXED in v0.2.1. `dna("S").gc_content()` → 1.0; `dna("NNN").gc_content()` → `missing`.
- D1 — '`try 1 + 1` gives an unexplained Record error': FIXED. The error now carries the precedence help (`try binds tighter than + … parenthesize: try (a + b)`). The precedence itself is unchanged — it stays in AGENTS.md as a syntax note, not a bug.
- '`helix test` says ok for a file that asserts nothing': FIXED. It now prints `FAIL … ran to completion without asserting anything`, rc=1.
- 3.2's exact program is invalid and its attribution is wrong: `lines.reduce("", concat)` does not compile (reduce demands an explicit two-binder lambda; String has no `concat` method — 24 methods, none is concat). The '150KB genome string build took 75s' does not reproduce for ANY string spelling on v0.2.1 (interpolation-fold at the claimed 148,890-byte scale: ~0.012s, ~5000x faster than claimed). The complexity-class complaint IS real — but it lives in array-of-strings accumulation (quadratic on all three engines, 256k pieces = 218.5s) and in the tree-walker (never got the v0.2.0 append fix at all).
- 'No efficient append' as an absolute: false for packed int/float arrays (linear since v0.2.0 — 1.02M appends in 44ms on the released binary) and false in general (`join("")` is linear everywhere: 8MB from 256k lines in 14ms, and the `+`-on-strings error already steers there).
- doc-verb's premise 'print owner + signature + doc line — the registry/signatures modules know them': FALSE as stated. There is no printable signature or doc-line metadata anywhere — BuiltinDef is {path, pure}, method tables are bare name lists, and the checker's knowledge (src/types/signatures.rs) is procedural match arms, not strings. A reverse lookup can print owners + effect + example receiver, not signatures, without building a new metadata table first.
- parse-helps items c, d, e, g are all stale — each already ships the asked-for house-pattern hint: (c) `"a"+"b"` → interpolation/join hint; (d) two bindings on one line → 'each statement goes on its own line'; (e) `;` → same, and here the source really has the semicolon; (g) `rec.f(3)` → the excellent '(rec.f)(…) — or bind it first' hint that even checks the field holds a Function.
- (Session-memory staleness, affects planning) 'helix doc is future work': a type-listing `helix doc [Type]` / `helix doc builtins` shipped and works on v0.2.1 — which is exactly why the doc-verb and parse-helps fallback pointers are implementable now.

## Do now — small, verified, low-risk (in order)

- 1. REPL banner discoverability (entry-points a): extend the fn repl() println at src/main.rs:1369-1372 with the `helix help` / `helix doc [Type]` / `helix describe` lines copied verbatim from print_help() (main.rs:883-884). Near-zero risk (tests/cli.rs:790 pins only exit-0 + '42'); add a pin test asserting the banner mentions 'helix describe' so discoverability cannot regress.
- 2. AGENTS.md at repo root (entry-points b): pure addition, no code path reads it. Draft is ~90% written; complete the truncated 'Footguns' section from the five live-verified footguns (missing-filter, i64 wrap, sum/reduce divergence, float ==, JIT cliff) per the plan below.
- 3. `helix test <file>` doc-module parity (test-file-dir): in cli_test (src/main.rs:1006-1011), don't push a definitions-only, doc-example-bearing, non-*_test.helix file into `files` — naming a file must mean what naming its directory means. Fully drafted, self-contained, fixes a self-contradicting FAIL. Land with the drafted regression test.
- 4. Dict/Record `expect(k)` (missing-provenance): the loud companion to `get` — raises at the lookup with a one-edit did-you-mean over the dict's own keys, before a `missing` is ever minted. Two match arms + two registry entries; zero checker/VM/JIT work (single call_method seam, JIT never sees dicts). Grep tests for 'available Dict methods' snapshots before landing.
- 5. `helix doc <name>` reverse lookup (doc-verb): when the query isn't a type, report every owner type + effect + example receiver + `helix doc <Type>` drill-ins, then fall through to the shared suggester. CLI-only; MUST update tests/cli.rs `doc_lists_methods_by_type` (pins 'unknown type' on stderr) in the same commit. No signatures — that metadata doesn't exist (see do_later).
- 6. parse-helps gaps a/b/f: (a) prefix_sum/cumulative_sum alias → 'a prefix sum is `xs.cumsum()`; the general running reduction is `xs.scan(init, f)`' + point the unknown-method fallback at `helix doc {type}` in BOTH twins (types.rs:280 and interp/methods.rs:3226, byte-identical); (b) interp_hole_hint for an undefined bare Ident inside a string hole, guarded by `if e.hint.is_some() { return e }`; (f) a Tok::Fn arm in do_block before primary()'s misleading 'expected a value here' catch-all. All pre-engine or twin-edited, so parity holds by construction; fail-on-0.2.1-first tests per house practice.
- 7. string-append FIX 1 — the Values arm in Value::concat_in_place (src/value.rs:584): makes the real genome case linear on the default engine (measured shape: 256k string appends 218.5s → ~0.05s). Last in this list because it is the only item where representation is semantic: the non-numeric-witness guard makes repacking impossible by construction, but land it ONLY with the repr-equality pins across all three engines plus the Values-all-ints+strings fuzz case.

## Do later — real features needing design

- `helix describe` signature enrichment (describe-sigs): probe the checker's own tables (types/signatures.rs builtin_type + per-receiver fns) with Type::Unknown vectors to derive arity and returns — sound today, arm-by-arm verified — plus hand-authored `params` names in registry::BUILTINS (131 entries) drift-pinned to the probe by a types/tests.rs test. Needs decisions: `arity: null` honesty for the 15 unguarded builtins (signatures.rs:780-795); universal_methods strings→objects compat; `returns: null` for comprehension verbs and Dict/Net.
- string-append FIX 2 — Op::AppendStrIntoLocal fast path for `"{acc}{x}"` folds on VM/JIT: a new opcode (ops.rs, hbc.rs name table, every exhaustive Op match) with byte-for-byte MAX_STRING_LEN error parity, non-Str-first-iteration fallback, and format-spec-hole decline. MUST coordinate with the in-flight uncommitted work in src/bytecode/comprehensions.rs (+57) and src/jit.rs (+138) — anchor by fn name, not line.
- string-append FIX 3 — the tree-walker never got the append wall fix (256k int appends still 6.3s, quadratic; matches the pre-fix number): the walker's reduce rebinds acc so the Rc is always shared. Touches the carefully-commented binder save/restore choreography ((a,a) same-name binders, error-mid-fold restore) — decline the fast path for same-name binders; needs its own design pass.
- DataFrame missing-filter semantics (live footgun 1): `where(@v == missing)` silently returns 0 rows, `.is_missing()` unsupported in queries, no `drop_missing` — the fix plan already slates 'work or error'. This is query-semantics design work under ADR 0001, not a patch. **STATUS: resolved the "work" way** — `.is_missing()` is admitted inside queries and frames have `drop_missing()`; `@v == missing` deliberately keeps selecting nothing, in agreement with arrays (see the `ColExpr::IsMissing` note, src/backend/mod.rs).
- Printable signature/doc-line metadata table (follow-up to describe-sigs + doc-verb): a genuinely new source of truth so `helix doc scan` can print a signature and doc sentence. Larger change; only after the probe-based describe enrichment proves the shape.
- Dict.get(k, default) parity with Record.get (which already takes a default) — small code, but an API-surface decision; fold into the errors-as-values (ADR 0004) discussion since `expect` will migrate there too.
- D2 `--explain-jit` / HELIX_JIT_EXPLAIN=1 — already deliberately deferred to v0.2.2 with a sketch in docs/v0.2.1-fix-plan.md; AGENTS.md documents the cliff until then.
- AGENTS.md rot-prevention: a cli.rs test that runs AGENTS.md's own command examples (or at least pins its claims that have binaries behind them), since two of its footguns are scheduled to be fixed and nothing fails today if the file goes stale.

## Declined, and why

- Provenance-carrying `missing` (payload on Value::Missing) or changing `missing == missing`: blocked by design and physics — ADR 0001 mandates one semantics over two representations, the Arrow validity bitmap has no payload slot (polars.rs round-trip would evaporate scalar provenance), Value is deliberately 16 bytes, and there are 232 Value::Missing sites across 21 files. `??`, `.has(k)`, and the new `expect(k)` cover the actual failure modes.
- A strict-lookup/strict-arithmetic env var: it would be the first HELIX_ variable that changes a program's computed answer (all existing ones are semantics-preserving — verified by grep), forking the language into dialects and breaking the three-engines-one-answer story at its root.
- Changing silent i64 wrap or the sum()/reduce divergence: documented as deliberate in docs/integer-semantics.md ('Wrapping is silent — no trap, no error, no promotion'). Document in AGENTS.md; revisit only inside the errors-as-values design, not as a DX patch.
- Changing `get`/`d[k]` missing-on-absence defaults: pinned by ADR 0001/0020 comments and existing tests; the reviewer's own framing conceded defaults stay. `expect` is the companion, not a replacement.
- parse-helps c/d/e/g: already ship correct, house-pattern hints (verified live) — touching them is churn with regression risk and zero user benefit.
- Adding a String `concat` method to make the review's exact program run: the program is foreign-idiom; the `+`-on-strings error already teaches the two linear idioms (interpolation, join), and one-obvious-way says don't add a third spelling that would then need its own fast path.

---

# DX Plan — seven-auditor synthesis (2026-08-15)

Source: seven audits of an external DX review, each verified against the **installed v0.2.1 binary** (`/home/areeb/.local/bin/helix`, reports `helix 0.2.1`) and the live repo. The reviewer wrote against an older binary in places — the stale claims are listed at the end and must not be re-litigated.

**Tree state at planning time (verified):** HEAD is `b293985` ("`unique` on a packed array no longer boxes"), one commit past the v0.2.1 tag, with **uncommitted work** in `src/bytecode/comprehensions.rs` (+57), `src/jit.rs` (+138), `tests/cli.rs` (+37). Consequences: (1) anchor every edit by **function/test name**, not by the line numbers below — they were taken mid-drift; (2) FIX-2 of the string item touches `emit_reduce_body_and_store` in the same file as the in-flight work — coordinate or land after it.

**Standing rules (non-negotiable):** no regressions; three-engine parity is sacred — byte-identical output, values AND error text, on JIT / VM (`HELIX_NOJIT=1`) / walker (`HELIX_NOVM=1`); every new error message follows the try-precedence-hint house pattern (test on the AST node, never displace an existing hint via `if e.hint.is_some() { return e }`, explain the rule AND show the fix); every fix ships with a regression test **confirmed to fail on the v0.2.1 binary first**; run `scripts/gate.sh < /dev/null`, never `cargo fmt`, never fat-LTO test runs.

---

## DO NOW (ordered by leverage / risk)

> **STATUS 2026-08-15: all seven landed in one change**, each pinned by a test in
> `tests/cli.rs` (`repl_banner_points_at_help_doc_and_describe`,
> `helix_test_on_a_doc_module_file_matches_the_directory_run`,
> `expect_is_the_raising_lookup_on_dict_and_record`,
> `helix_doc_reverse_looks_up_methods_and_builtins`, `parse_help_gaps_are_closed`,
> `string_fold_matches_plain_concat_in_value_and_representation`). Item 7 measured
> 235.8 s → 61 ms at 256k pieces on the default engine. Two sites the plan missed,
> found while landing: the checker's own record-method arm (`types/signatures.rs`,
> `record_method_type`) needed `expect` too, and its unknown-method hint text now
> names it.

### 1. REPL banner mentions help/doc/describe (entry-points 1.1)
**Verified state:** bare `helix` prints exactly one banner line + prompt; never mentions `help`, `doc`, or `describe`. Banner at `src/main.rs:1368-1372` in `fn repl()`; the text to echo lives in `print_help()` at `main.rs:866-891` (doc/describe lines at :883-884).
**Change (drafted, complete):**
```rust
println!(
    \"Helix {} — interactive session. Type an expression and press Enter; Ctrl-D to exit.\\n    \\
     helix help               commands and usage\\n    \\
     helix doc [Type]         list a type's methods (Array/String/Dna/…) or `builtins`\\n    \\
     helix describe           the whole API as JSON (for LLMs/agents/tools)\",
    env!(\"CARGO_PKG_VERSION\")
);
```
The doc/describe lines are copied **verbatim** from `print_help()` (same `\\n    \\` continuation, same 25-column alignment) so the two surfaces cannot drift stylistically.
**Risk:** near-zero. The only REPL test, `tests/cli.rs` `repl_evaluates_and_exits_on_eof` (~:790), pins exit-0 + `stdout.contains(\"42\")` only. Line 1 of the banner is unchanged (external scrapers safe); stays on stdout.
**Pin:** add a cli.rs test asserting the REPL banner contains `helix describe` — makes the discoverability fix regression-proof.

### 2. AGENTS.md at repo root (entry-points 1.4)
**Verified state:** no AGENTS.md, no CLAUDE.md; `ls` confirms. Pure addition — `helix test` scans only `*_test.helix` + `## >>>` examples (verified live), so no code path reads it.
**Change:** create `/home/areeb/projects/helix/AGENTS.md` from the entry-points auditor's draft. Sections already written: self-describing binary (describe/doc/check/test/eval/fmt), syntax-that-trips-agents (fn `=` bodies, if/then/else, no string `+`, no `\\u` escapes, try precedence), three-engines correctness model with the differential-test recipe and \"any divergence is a Helix bug — report it, never code around it\".
**Complete the truncated 'Footguns — wrong answers, not errors' section** from the five live-verified footguns:
1. **missing-filter:** `where(@v == missing)` → 0 rows silently (because `missing == missing` is `missing`); `.is_missing()` unsupported inside queries; no `drop_missing`; the working keep-non-missing idiom is `where(@v == @v)`. (Fix slated 'work or error' in docs/v0.2.1-fix-plan.md:224-247.)
2. **Silent i64 wrap:** `9223372036854775807 + 1` → min-i64, rc=0 — deliberate per docs/integer-semantics.md:12-27.
3. **sum vs reduce divergence:** `[i64::MAX, 1].sum()` → `9223372036854775808.0` (float) but the reduce spelling wraps — documented 'Known divergence', integer-semantics.md:50-62.
4. **Float `==`:** `[0.1,0.2].sum() == 0.3` → false; use `assert_close`.
5. **JIT cliff is silent perf, never wrong answers** — `--explain-jit` lands v0.2.2 (fix-plan STATUS item 5).
**Risk:** rot only — footguns 1 and 5 are scheduled to be fixed and nothing fails if AGENTS.md isn't updated. Mitigation is the do-later rot-pin test; item 1's banner test covers discoverability now.

### 3. `helix test <file>` = `helix test <dir>` for documented modules (test-file-dir)
**Verified state:** same defs-only module with two `## >>>` examples — dir run: `2 passed`, exit 0; file run: FAILs it for asserting nothing **and** passes the same 2 examples in the same output, exit 1. Mechanism: file mode pushes the root unconditionally (`cli_test`, src/main.rs:1006-1011); dir mode collects only `*_test.helix` (`collect_test_files`, :1257-1275); vacuity arm at :1036-1061; `run_doc_examples` handles a file root (:1137-1143). No existing test invokes `helix test` with a file path (9/9 call sites pass dirs).
**Change (drafted, complete in the audit):** replace :1006-1011 — when the root is a file, skip pushing it iff (not named `*_test.helix`) AND `doctest::doc_examples_in(src)` nonempty AND `is_definitions_only(src)` (:1230). `files` empty ⇒ `any_doc_examples` keeps the honesty path silent, examples run, output becomes byte-identical to the dir run. A directly-named `*_test.helix` keeps the assert-or-fail contract (dir-mode parity for the pathological case).
**Risk:** confined to exactly the buggy shape (flips FAIL→dir-run result; the `running 1 test file` line disappears for it). Pinned neighbors `helix_test_fails_a_file_that_asserts_nothing` (~cli.rs:3064) and `helix_test_runs_the_users_own_doc_examples` (~:3103) are dir-mode, untouched. Cost: one extra read+parse of the named file, file-mode only.
**Pin:** the auditor's drafted test `helix_test_on_a_doc_module_file_matches_the_directory_run` — insert after `helix_test_runs_the_users_own_doc_examples` (anchor by name; cli.rs has drifted).

### 4. `expect(k)` on Dict and Record — the loud lookup (missing-provenance, option c)
**Verified state:** the laundering reproduces on v0.2.1, all three engine configs — a typo'd key flows through `*`, `+`, `print` with exit 0. `get` is strictly 1-arg and missing-on-absence (interp/methods.rs:547-550); `d[k]` likewise (access.rs:363-366); no raising lookup exists. Dispatch is one seam: vm.rs:1443 and interp.rs:664 both route through `interp::call_method`; **grep `Dict` in src/jit.rs = no matches**; checker treats Dict as Unknown (signatures.rs:869-871) — so zero VM/JIT/checker work and no parity surface.
**Change (drafted in the audit):**
- registry.rs:310/:317 — add `\"expect\"` after `\"get\"` in DICT_METHODS and RECORD_METHODS (the unknown-method help text is generated from these lists — updates itself).
- interp/methods.rs — `\"expect\"` arm after dict `get` (:550): arity(1); hit ⇒ clone; miss ⇒ HelixError `key \\`{k}\\` not found in this dict (N keys)` + one-edit did-you-mean over the dict's **own string keys** via `crate::error::typo_distance` (error.rs:164 — the house one-edit policy), else the fallback hint naming `.has(k)`, `.get(k)` → missing, `.get(k) ?? default`. Record twin after :645 (the audit's EDIT 3 was truncated — same shape as the dict arm, field-name key).
- ADR 0001 untouched: `missing` the value still propagates; `expect` raises on the **miss**, before a `missing` is minted.
**Risk:** low. (1) grep tests for `available Dict methods` snapshots first; (2) did-you-mean stays within one edit (\"a wrong suggestion is worse than silence\"); (3) name is deliberately forward-compatible with ADR 0004 errors-as-values (`expect` migrates raise→Err); (4) do NOT touch `get`/`[k]`.
**Pin:** interp/tests.rs next to existing dict/record method tests — hit, miss+near-typo (hint text), miss+no-near (fallback hint), arity error; run on all three engines.

### 5. `helix doc <name>` reverse lookup (doc-verb)
**Verified state:** `helix doc scan` → `error: unknown type` exit 1, while `scan` is a real Array method. `cli_doc` at main.rs:263-309 (error at :299-304). Suggester exists and is callable (`suggest::hint`, suggest.rs:262; `mod suggest` at main.rs:46). Multi-owner is real (`mean` on Array/Tensor/GroupBy; `max`/`min`/`count` also builtins) — report ALL owners, both namespaces. Printable per-name metadata that EXISTS: `capability::effect_of`/`method_effect_of` + `Effect::label`, `registry::category_of`. **No signatures/doc lines exist anywhere** — do not attempt to print them (see do-later).
**Change (drafted in the audit, 3 edits):**
1. suggest.rs:150 — `receiver_for` → `pub(crate)` (one source of truth for example receivers).
2. main.rs `Some(query)` arm of `cli_doc` — type match first (case-insensitive — preserves the `dna` type-vs-builtin collision), then: owners scan over `type_method_tables()` → `\\`scan\\` is a method on: Array (effect: pure)` + `e.g. \\`xs.scan(...)\\` — full method list: \\`helix doc Array\\``; UNIVERSAL_METHODS check; `registry::lookup` for free functions (effect + category + `see helix doc builtins`); any hit ⇒ exit 0. Unknown everywhere ⇒ route through `suggest::hint` (aliases first, then one-edit typos), same suggester every \"is not defined\" error uses. (The audit's EDIT 3 — the final unknown-name message — was truncated: write it to still contain a recognizable phrase, or keep `unknown type` wording to zero test churn.)
3. tests/cli.rs `doc_lists_methods_by_type` (~:1789-1809) — :1805-1808 pins `doc Nope` → exit 1 + stderr `unknown type`; **must be updated in the same commit** unless the zero-churn wording is kept. `doc Dna`/`doc array` assertions unaffected (type branch wins first).
**Risk:** CLI-only, no engine semantics. Dual-namespace names now print two lines (intended stdout-shape change). `method_effect_of` is keyed by name alone — one effect label even for multi-owner (matches `describe`). `doc sacn` (2 edits) still gets silence — deliberate policy (error.rs:154-163), not a gap. No script/doc greps the old error (verified).
**Pin:** new cli.rs cases — `doc scan` (method, exit 0), `doc sqrt` (builtin), `doc max` (both namespaces), `doc len` (alias hint), `doc zzz` (miss, exit 1).

### 6. Three parse-help gaps — a, b, f only (parse-helps)
**Verified state:** (a) `xs.prefix_sum()` → 79-name dump, no steer (fallbacks: types.rs:276-282 compile-time, interp/methods.rs:3211-3231 runtime twin); (b) `\"{feat}\"` with `feat` undefined → no help at all (checker Interp arm types.rs:523-532, Ident error :571); (f) `fn` inside `do { }` → misleading `expected a value here` (do_block parser.rs:2109-2170 falls to primary()'s catch-all :2637-2642; `mut` and `in` already have sibling special cases at :2117-2120, :2144-2151). Items c/d/e/g verified already-good — **leave alone**. No test pins any text being changed (grepped).
**Change (drafted in the audit):**
- (a) suggest.rs ALIASES (~:114): `prefix_sum` and `cumulative_sum` → Target::Text \"a prefix sum is `xs.cumsum()`; the general running reduction is `xs.scan(init, f)`.\" Plus, in **both twins byte-identically** (types.rs:280 AND interp/methods.rs:3226): replace the 79-name dump fallback with `no similar method — \\`helix doc {type_name}\\` lists all {type_name} methods.` Update the stale parenthetical at types/synth.rs:291. Conservative fallback if the dump is deemed load-bearing: land the alias only.
- (b) new `interp_hole_hint` helper next to `field_on_non_record` (after types.rs:274), modeled on `try_binds_tighter_hint`: fires only for a bare-`Ident` hole, only when `e.hint.is_none()`; text explains braces-are-interpolation, shows the two fixes (define/spell the value, or `{{feat}}` for literal braces — the escape exists, lexer.rs:586, and lexer hints :714/:786 already teach it). Wire via `.map_err` in the Interp arm (:523-532). (Draft truncated mid-hint-string — finish the sentence naming `{{…}}`.)
- (f) a `Tok::Fn` arm in `do_block` before the fallthrough, sibling to the `mut` case: explain that `fn` is item-level, and show the local binding form (`f = (x) => …`) per house pattern.
**Risk:** parity by construction — (b)/(f) are checker/parser (run once, pre-engine); (a) edits both twins with identical format strings and the shared alias table. The alias also fires for `s.prefix_sum()` on String/Dna receivers — same accepted behavior as existing entries. Verified untouched pins: types/tests.rs:84/:125 (`needs numbers`), vm/tests.rs:6038 (scalar-receiver fallback is separate, types/synth.rs:330).
**Pin:** fail-on-0.2.1-first tests for each of the three hints, all three engines for (a)'s runtime twin.

### 7. string-append FIX 1 — Values arm in `Value::concat_in_place` (src/value.rs:584)
**Verified state:** array-of-strings fold `lines.reduce([], (acc,s) => acc.concat([s]))` is textbook-quadratic on ALL THREE engines of released 0.2.1 (n=64k: 13.1s; n=256k: 218.5s; ×4 n → ×16.6 t). `ConcatIntoLocal` fires but `concat_in_place` only extends for `(Ints,Ints)`/`(Floats,Floats)`; a `Values` accumulator falls to `to_values() + array_sniff` = O(n) per step. This is the reviewer's real 75s-magnitude complaint (mis-attributed to strings). The linear idiom exists (`join(\"\")`: 8MB in 14ms) but the fold spelling should not be a trap.
**Change (drafted, complete):** add after the two packed arms inside the existing `match (Rc::get_mut(&mut cur), add)`:
```rust
// A result containing a non-numeric element can never repack: `array_sniff`
// returns `Values` unless ALL elements are Int or ALL are Float. So when the
// accumulator is already `Values` and the argument carries a non-numeric
// witness, extending in place is representation-identical to the ordinary
// `to_values` + `array_sniff` path. The guard is O(|add|), the payload.
(Some(ArrayData::Values(v)), add)
    if add.to_values().iter().any(|x| !matches!(x, Value::Int(_) | Value::Float(_))) =>
{
    v.extend(add.to_values().iter().cloned());
    return Value::Array(cur);
}
```
(Bind `add.to_values()` once if the double call bothers review — in the fold path it is a borrow.) A numeric-only `add` still declines (an all-int result must sniff to `Ints`). Update the fn's \"deliberately narrow\" doc comment (:572-583) to name the new arm's invariant. Expected: 256k string appends 218.5s → ~0.05s on default engine.
**Risk (the one real risk in do-now):** representation is semantic — `array_sniff` repacking decides later packed-kernel eligibility, and the three-engine oracle counts any divergence. The witness guard makes repacking impossible by construction; the failure mode to guard in review is accepting a numeric-only `add` into a Values acc. Do NOT land without the pins.
**Pins (mandatory):** (1) VALUE **and REPR** equality between the fold spelling and plain `a.concat(b)` on mixed/str/record elements, all three engines; (2) fuzz case: acc = Values-holding-all-ints + string `add` (repr must be Values both ways); existing HELIX_SOAK fuzz + `scripts/stranger.py` + doc-example oracle as backstops. Timing assertions not required — repr/value equality is the pin (wall clock here is ±15%; the complexity verdicts rest on ×14–×21 ratios, far outside noise).

---

## DO LATER (real features — design/ADR first)

### Recorded by the v0.5.1 correctness sweep (2026-08-24)

What the sweep found but deliberately did NOT fix in a patch release, with the
mechanism so nothing has to be rediscovered:

- **Desugar blame-name threading.** Errors inside desugared sugar blame the
  desugar target, not what the user wrote: `count_where(pred)` errors say
  `filter`, `zipmap` errors say `zip`. Fix needs a blame channel on the
  synthesized nodes (a `blame: Option<String>` the error path prefers), touching
  parser desugars + both engines' error paths. Message-only in effect but
  pin-heavy; do it in one sweep with the pins updated together.
- **Named arguments on selectively-imported functions.** `import m.{f}` +
  `f(x, punct: "?")` now gets an honest parse error (the signature lives in
  another file), and positional defaults fill at load time — but full support
  needs `Expr::Call` to CARRY named pairs to the module loader (today only
  `Expr::Method` does), i.e. an AST change + parser deferral for names in
  `selected_imports`. Surface addition → 0.6.0.
- ~~**Static/runtime error family drift.**~~ **DONE 2026-09-02.** The checker now
  speaks the runtime's sentence, and a test holds the property rather than a convention.

  **The estimate was wrong in a useful direction: four pins, not ~40.** Most of what
  `grep "has no method"` matched was the RUNTIME's sentence, which did not move — the
  unification brought the checker to the form the majority already spoke. The runtime's
  form won on evidence in the tree, not taste: the sibling family ("`f` is an Int, not a
  function") already ran `with_article` on both sides, so an article was the house style
  and "type Int" was the outlier.

  **What made it hard was not the count.** Enumerating — forcing the same refusal through
  a LITERAL receiver (checker) and a PARAMETER receiver (runtime) and diffing the columns
  — found three things grep did not:

    * `types.rs::unknown_method` was one of THREE checker producers. Fixing it left
      `Array` and `String` agreeing while `Int` still diverged, via
      `types/synth.rs`'s scalar fallback, and `Record` via a hardcoded string in
      `signatures.rs`.
    * `interp/comprehensions.rs::not_an_array` is a RUNTIME path that spoke the CHECKER's
      sentence. Unifying the checker alone would have INVERTED the drift for
      `x.map(it)` rather than closing it.
    * Three more sites built the right words by hand (`a Tensor`, `a DataFrame`,
      `a Connection`) — correct only because someone typed the right article.

  `a_refusal_reads_the_same_from_the_checker_and_the_runtime` is the enumerator kept as a
  guard. Sabotaged three ways, each producer caught by a different case.

  Still divergent and NOT part of this, because it is a different question: the checker's
  argument-vs-receiver order. `(5).map(it)` refuses the method, `"s".map(it)` refuses the
  unbound `it` first. Same program shape, different first complaint by receiver type.

  **Sized and motivated 2026-09-02.** About 40 pins across 14 source files plus four
  corpus goldens (`grep -rn "has no method"`). The case for doing it is no longer
  aesthetic: the Tuple gap hid inside this drift. Teaching the RUNTIME that a tuple has
  `count()` left `(1, 2).count()` still failing while
  `{a: 1}.items().map(it.count())` worked — the second receiver is Unknown so the checker
  waves it through, the first is typed `Tuple` and the checker had no arm. The only signal
  saying which half was speaking was the article. Two families meant the fix looked applied
  when half of it was not.

  A grammar bug inside one half was fixed separately (`a Array` → `an Array`); that one is
  not the drift, and fixing it does not reduce the drift.
- **Polars-side tightenings** decided in ADR 0034's addendum (bool `sum`
  refusal, exact-case Bool inference, ragged-row refusal, duplicate-header
  refusal) — each narrows polars-backend behavior to the native doctrine, so
  each is a minor-version change.

### Recorded by the 2026-08-19 stabilization sweep (lower tier — engine-identical, no oracle divergence)

The sweep's top tier (poison-cell Let arm, autodiff broadcast/exponent/stale-grad,
join order, test-walk cycles) landed in `79f4f40` + `62716f1`. What remains, recorded
with mechanisms so nothing has to be rediscovered:

- **Grouped i64 sum silently WRAPS at the Polars seam** while the column/array path
  promotes to float on overflow: `df.group(@g).sum(@v)` on two `4611686018427387904`
  rows answers `-9223372036854775808` where `df.column("v").sum()` and `[big, big]
  .sum()` answer `9.22e18`. Same word "sum", answers differ in sign; oracle-blind
  (engine-identical). Needs a seam policy decision — Polars has no checked sum, and
  pre-casting to f64 changes small-int dtypes. Candidate: dual-agg detector (i64 +
  f64 sums, promote when they disagree materially) — decide deliberately, not inline.
- **Missing-ordering seam contradiction**: frame `.sort(@k)` accepts `missing` and
  sorts it FIRST; array `.sort()` refuses ("the array has missing values"). ADR
  0025's array-side refusal has no frame-side counterpart. Policy decision.
- **Zero-row / all-missing columns infer dtype `str`**, so grouped numeric verbs
  error on empty CSVs with leaked Polars wording ("sum` operation not supported for
  dtype `str", unbalanced backticks) — while a frame emptied by `where()` keeps
  dtypes and aggregates cleanly. Also the duplicate-output-name path leaks Polars'
  "duplicate: column with name ..." with no Helix concept named. One family: seam
  errors need a translation layer at the schema-read boundary (src/backend/polars.rs).
- **`where(@v == missing)` / `!= missing` still silently return 0 rows** (v0.2.1
  finding, unchanged in v0.2.6). `drop_missing()` is the sanctioned spelling; the
  equality spellings should ERROR with a hint, not silently match nothing.
  **STATUS: resolved differently** — `.is_missing()` is now admitted inside queries
  and frames have `drop_missing()`; the equality spelling deliberately keeps
  selecting nothing (ADR 0001 agreement with arrays; `ColExpr::IsMissing`,
  src/backend/mod.rs) rather than erroring.
- **`strip_mangling` corrupts user strings shaped like `m<digits>$`** in multi-file
  error renders (src/main.rs:1554→1468): a dict key `m5$gone` in an `expect()` error
  is reported as `gone` — but only when an import exists. Real fix is demangling
  identifiers where they are INSERTED into messages, not post-hoc over the whole
  render; touches every error-construction site that embeds a fn/var name. Corner
  case (user data containing the mangle shape), but the did-you-mean surface can
  name a key the user never typed.
- **Parse-time import-name check ignores scope** (`self.imports.contains(n)`,
  src/parser.rs:1703): rebinding a module name to a value leaves sugar-named method
  calls resolving to the MODULE (wrong answer, rc 0: `mymod.sort_by(9, 4)` → `5`
  after `mymod = [3,1,2]`), and a lambda/fn parameter named like an import suppresses
  array sugar with a self-contradicting "did you mean `sort_by`?" error. Needs
  binding-aware resolution (parser scope tracking or a resolver pass) — same family
  as ADR 0026's name-resolution work.
- **reduce/scan both-bad error-wording asymmetry** (fold fast path validates the
  argument before taking the accumulator; the general path reports the receiver
  first). Byte-identical on every engine — a wording-choice gap in ADR 0029's
  error-text pin only. Extend the pin's wording note if the fast-path set grows.
- **Autodiff DX family** (engine-identical, non-blocking): tracked error paths drop
  the help hints their plain twins carry (misaligned tracked matmul loses the
  "vector-vector, matrix-matrix..." help); unary minus on a Node errors where
  `0.0 - x` works. Also: a tracked element in an array `.sum()` switches the fold
  from compensated to naive left-to-right, observably changing the sum of the same
  data (~1e-12 at n=1000) — docs note or a compensated tape fold. *(Differentiable
  indexing landed with the bridge; the `.exp()` asymmetry is now its own entry
  below, because the bridge made it reachable by an ordinary spelling.)*

### The parser's UFCS rewrite — the last parse-time decision — is gone (2026-09-02)

**DONE**, and it closes the PyObject residue recorded under "UFCS after the PyObject
narrowing" below: the receiver decides, so a PyObject receiver takes the method path
because it is one.

What the removal taught, in the order it was learned. Deleting the rewrite alone fixed
every semantic edge — the field reading, precedence, three engines agreeing — and measured
`it.f(1)` at **108 ns against 25**: the JIT's kernel analysis admits a `Call` and not a
`Method`, so the comprehension stopped fusing. The layer that KNOWS the receiver's type has
to decide there, not defer to run time out of principle. `src/ufcs.rs` runs after the
checker and rewrites only what the type proves; `jit-explain` went from "0 kernel sites
offered" back to 1. The runtime route then had a 40 ns tail on a Record receiver — a
fallback predicate hashing two strings per call for a fact fixed per call site — which
became a per-site owner list, a peek instead of a pop, and an interned name. Same binary,
interleaved min-of-15: Int 0.976×, Record 1.021×. Recorded in ADR 0045's addendum.

### A function-valued field was not callable as `rec.f(x)` (2026-09-02)

**FIXED.** `(rec.f)(x)` worked; `rec.f(x)` was refused by both halves with a hint that
named the problem exactly ("the object-API spelling `r.go(3)` is what everyone writes
first"). Reported from the field as §1.27 #2 — the rule that blocked `User.find(1)`.

The only real question was precedence, and the code had already answered it: the five
real Record methods are matched before any field fallback, in both the checker and the
runtime. So the order is real method → function-valued field → UFCS, each boundary pinned
from both sides in `a_function_valued_field_is_callable_with_method_syntax`.

### A join type could not come from a binding (2026-09-02)

**FIXED.** `join` read the type only as a trailing string LITERAL, so a bare name was
always a key and a library had to branch over five constants. Reported from the field as a
standing workaround ("attach branching on a literal join type").

The filing first read as an ambiguity between key and type; the probe that settled it
pinned the key and failed identically, so a bare name in that list is simply always a key.
The fix is a trailing options record, `{how: kind}` — the idiom `http_request` already
uses. Deciding the role from the VALUE was rejected on purpose: `l.join(r, k1, k2)` with
`k2` = "left" and no such column is a clean error today and would have become a silent
left join on `k1` alone.

A string literal before the last argument became a key at the same time, which `select`
had always accepted and `join` had not.

### A frame has no `rename`, and no dynamic column REFERENCE (2026-09-02)

**`rename` LANDED 2026-09-02.** It needed no new syntax and no seam change in the
end: two NAME positions, both ordinary evaluated strings, and a *provided* trait
method composing `with_columns` + `select` — so both backends agree by construction
rather than by two implementations matching. The estimate below ("a MINOR and an
hour of careful work") was right about the registration sites and wrong about the
backends: reading the seam showed `unique` and `column` already take evaluated
string arguments, which is the pattern `rename` wanted, not `select`'s.

The dynamic column REFERENCE below is still open, and is now the only half left —
though with `rename` in place nothing has asked for it.

With `with`'s key and `join`'s keys now taking a binding, one gap is left and it is
narrower than it first looked. Those are NAME positions, where ADR 0028 makes a binding
name a column. An EXPRESSION position is the opposite by the same rule — a binding is its
value — so

    fn rn(f, to, from) = f.with({to: from})    # from = "author_id"

writes the literal string, not the column. That is ADR 0028 being consistent rather than a
bug: `@author_id` pins a column in an expression, and there is no `@{from}` spelling for
"the column this binding names".

The operation actually wanted is a **rename**, and a frame has none. `df.rename(old, new)`
would be two NAME positions, so it needs no new syntax at all — just the rule that already
exists. What it does need is a verb across the ADR 0012 seam: the trait method plus both
backends, plus the nine registration sites and a dfdiff corpus program. That is a MINOR and
an hour of careful work, not something to take on the way past a bug fix.

It blocks a generic relation-attach in a library: aligning a child's foreign key to a
parent's key needs the rename, and every other part of that join now works.

### A `with` column cannot be named at run time (2026-09-02)

`select` honours a binding in scope as a column name (ADR 0028). `with` does not, and
cannot, because its argument is a RECORD and a record literal's field name is syntax
everywhere else in the language:

    K = "score"
    d.with({K: @a * 2})     # a column literally named "K", silently

**FIXED 2026-09-02** — `with`'s key and `join`'s keys now resolve through the same rule
`select` uses. ADR 0028 did not decide this; it named it as an OPEN QUESTION ("does the
same rule apply to the name being DEFINED, or only to names being read?") and shipped the
read positions only. This answers it, the same way and for the same reason, and covers the
join key the ADR never reached. The entry
below is kept for the reasoning about `dataframe(dict)`, which turned out NOT to be the
answer: a Dict's values are evaluated, and a `with` value is unevaluated column syntax
(`@x * @y`), so the Dict form cannot carry it. Resolving the KEY was the answer instead.

All three engines agreed on the old behaviour, so this was not a divergence — it was a
missing capability with a surprising default, and a wrong answer rather than a refusal.

**The precedent is already set.** ADR 0043 hit exactly this for construction — "a column
could not be named at run time" — and answered it by having `dataframe()` accept a
**Dict** beside a record, columns in sorted key order because a Dict has no insertion
order to invent. `with(dict)` is the same answer to the same question, and it keeps the
record form meaning what a record literal means everywhere else rather than making `with`
the one place a field name is evaluated.

It is a language-surface addition, so it is a MINOR, and it is written down here rather
than taken on the way past: the release that would carry it should carry it deliberately.

### The fixed cost of a comprehension (2026-09-01)

A field build brought a query builder from 24.15 to 5.65 us against GORM's 2.76, and the
remaining gap is ours rather than theirs. Measured here, min-of-7, load 0.30,
`HELIX_THREADS=1`, control subtracted:

| shape, over a 2-element array | us |
|---|--:|
| `map(it).count()` | 0.145 |
| `filter(it > 0).count()` | 0.187 |
| `any(it > 0)` | 0.081 |
| `reduce(0, +)` | **0.029** |

And across sizes: `map` over 2 costs 0.137, over 20 costs 0.146, over 2000 costs 1.426 —
so **~0.135 us FIXED per comprehension and ~0.65 ns per element**, a ratio of 200. Below
about 200 elements a comprehension pays for its setup, not its work, which is why fusing
four passes into one `reduce` won that build 1.44x on a two-element array. That is the
reverse of the advice for a large array, and worth stating in the docs as its own rule.

WHAT IT IS NOT. Not the JIT: the same shapes under `HELIX_NOJIT=1` are 1.5-5x SLOWER
(`reduce` 0.029 -> 0.154), so the native path is already earning its place at n=2. Not
`ColumnBuilder` either — it is a small enum plus a counter, one allocation for two
elements, and glibc's tcache serves that in ~2 ns.

WHY IT IS NOT FIXED HERE. The remaining candidates (op dispatch across `CompInit` /
`CompNext` / `CompPush` / `CompEnd`, the fusion guard, the native call setup) differ by
30-150 ns, and this harness carries about 10% noise at that scale — the pinned control
moved as much as the subject in the guard A/B earlier this cycle. Choosing between them
needs a profiler on the Rust binary, not another Helix micro-benchmark, and an
unmeasured VM optimisation is exactly what this file exists to keep out of a release.

THE ARITHMETIC THAT MAKES IT WORTH DOING. A hand-minimal Helix renderer for that build's
exact query is 0.71 us — the language already beats GORM by 3.9x for the work itself. Of
their 5.65, roughly 1.2 us is comprehension setup (nine of them) and roughly 1.2 us is
record lookups. Removing ALL of the first still lands at 4.45, so this is necessary and
not sufficient: closing it needs both halves, and neither side should claim the win alone.

### UFCS after the PyObject narrowing (2026-08-20)

v0.3.0 shipped UFCS gated on `registry::is_any_method`, and called it strictly additive.
It was not. That table covers the NINE types with static method tables; a **PyObject**
resolves its attributes at run time and appears in none of them, so
`m = python.import("math")` then `m.sqrt(16.0)` was rewritten to `sqrt(m, 16.0)`, with no
shadowing at all, because `sqrt` is a builtin. `np.round(1.5)` became `round(np, 1.5)`,
which type-checks clean because `round(x, digits)` is a real two-argument builtin — the
silent-wrong-answer class, shipped by the change whose own commit message claimed it
could not happen. Found because a field build insisted on verifying the claim instead of
trusting it, which is the lesson worth keeping.

UFCS is now restricted to **user-defined functions**. What that leaves:

- **THE PROPER FIX, not yet built.** Decide on the RECEIVER at run time: a PyObject
  always takes the method path, everything else falls back to the function. The
  tree-walker can do this today at `interp.rs`'s `Expr::Method` arm. The VM cannot,
  because it has no way to invoke a function from inside `Op::CallMethod` — it needs the
  compiler to emit a branch, which needs a peek-the-receiver test opcode, which touches
  the bytecode format (`ops.rs`, `hbc.rs` sizing and serialisation, `vm.rs`, and the
  emitter). That is a real change and it should be made deliberately, not smuggled in
  behind a bug fix. With it, builtins could chain again — `tensor(t).to_array()`,
  `(0 - 1).abs()` — which is what the narrowing costs today.
  **STATUS: landed in full (2026-08-31, ADR 0045).** Half two shipped first — a
  builtin-named method call that fails dispatch retries the builtin on the receiver, on
  all engines (`ufcs_fallback_applies`; the VM's `ufcs_name` seam). The rest landed with
  the user's own functions, which is what actually mattered: `is_any_method` blocks
  `where`, `select`, `first`, `count`, `all`, `join`, `sort`, `take`, `drop`, `insert`,
  `get`, `keys`, `values`, `filter`, `map`, `sum`, `min`, `max`, `unique` — the entire
  vocabulary of a fluent library — so a query builder or an ORM could not be written in
  the language at all.

  The predicted opcode is real and is called `Op::ReceiverIs`. It did NOT touch the
  bytecode format: `hbc.rs` rejects `Op::Method` outright (value methods are outside the
  hvm core subset), and nothing else serialises a `Program`, so the sizing and
  serialisation this note feared do not exist for these ops. What it did need was the
  compiler emitting BOTH readings at a two-reading call site, which is where the work
  actually was.

  **What is still open, precisely.** The parser's own desugars run before any receiver
  exists and still win: `sort_by`, `min_by`, `max_by`, `argmin`, `argmax`, `take_while`,
  `drop_while`, `zipmap`, `flat_map`, `count_where`, `position`. A user's `fn sort_by` is
  therefore unreachable in method position. Closing it means either moving those desugars
  behind the same receiver test, or teaching the parser to decline a desugar whose name
  the file also declares — the second is smaller and the second is also wrong, because it
  would break `xs.sort_by(...)` in the same file. So: the receiver test, again, at seven
  more sites. Not urgent; none of these is a natural library verb.

  **NAMESPACING TURNED IT OFF, and the corpus could not see that.** `module::load`
  rewrites every top-level name to `m<N>$name` once a second file is involved, and the
  fallback looked its target up by the name at the CALL SITE — which for a method is the
  written one, because a method name must not be rewritten. So the feature was live in
  exactly the files that do not need it and absent from every file that does; a library's
  consumer always has an import. Fixed by carrying the resolved name on the call site
  (`Expr::Method::ufcs`, filled by the loader). `climain::find` had the same bug for
  `fn main` and is fixed with it. The lesson is the test, not the fix: no corpus program
  has an import, so a single-file corpus was blind to every multi-file program by
  construction — `an_import_does_not_disable_ufcs_or_fn_main` is the multi-file guard.

  **A FRAME REACHING A ROUTE CHOSEN BY TYPE — closed 2026-09-01, and it was worse than
  first recorded.** Every verb a frame or a group routes by TYPE guessed when the checker
  could not prove the receiver, and the guess evaluates the arguments as values where a
  frame verb's arguments are column names. So this ran on one engine and died on two:

      fn adults(f) = f.where(age > 40)   # bare column names: ADR 0039's own spelling
      # walker: 2        VM / JIT: `age` is not defined

  Not a curiosity — that is the shape a database helper is written in, and a `@column`
  argument was the only hint that switched the route. The same guess was in `sort`,
  `drop_missing`, `drop_nan` and every grouped aggregation (`g.mean(v)` → "`v` is not
  defined"), so the fix is the family: `ReceiverIs(DataFrameOnly)` / `GroupByOnly`, both
  readings emitted, decided on the value. Twenty measured cases across the two receivers
  now agree on all three engines, where twelve diverged.

  The fusion cost this note feared is **not measurable**, and the control says why: an
  unproven receiver never fused in the first place, because the chain analysis cannot see
  an array. Guard off → on, unproven/pinned ratio 1.335 → 1.341 at 4M elements. `map` is
  untouched regardless: a frame owns no `map`, so there was never a second reading.

  Six wording divergences went with it — a group refused after its arguments were
  evaluated (naming `v` rather than `sort`), `scan` reporting "no method `map`", and four
  sentences written twice. Each is one constructor now, called by both engines.

  **The other twin.** `Op::GroupByAgg` still raises "expected a GroupBy, got X" for a
  non-GroupBy receiver where the walker reports something else — the same divergence
  `Op::DfColumnVerb` had, which ADR 0045 fixed by calling the walker's own constructors.
  It was left because the walker's path for an aggregation on a non-group receiver
  evaluates the `@column` first and raises the column-reference error, so matching it is
  not the same one-line substitution.
- **The residue.** A user's own `fn` can still collide with a Python attribute they also
  call (`fn helper` plus `np.helper(...)`). It is a name they chose and can see, and it
  was an error before UFCS existed, so nothing that worked changes — but it is not
  nothing, and the run-time fix closes it too.
- **The half-resolution.** The field build's reading is correct: item 2.4 is now half
  answered. Functions can be called in method position; methods still cannot be called
  as free functions (`matmul(a, b)` fails). The reverse direction is a separate decision
  — it would make the two spellings genuinely interchangeable, which is a simpler rule
  than the one we have — and it should be argued on its own, not assumed because the
  forward direction shipped.

### Two questions the field build raised, answered (2026-08-20)

- **`http/` duplicating three native builtins** (`url_encode`/`url_decode`,
  `parse_cookies`, `parse_set_cookie`). The library versions keep their place: they do
  things the builtins deliberately do not — ordered pairs, a multi-map for repeated
  header names, `__Host-` prefix validation. The builtins exist because the naive
  version of each is wrong in a way that looks right (percent-encoding is over BYTES,
  and a `Set-Cookie` `Expires` contains a comma). **Recommendation:** the library keeps
  its own names and documents the relationship at the top of each module — "the builtin
  does X; this adds Y" — rather than either side being removed. A wrapper that only
  forwards would be the second spelling ADR 0003 forbids; a wrapper that adds structure
  is a different verb doing a different job.
- **`http/status.helix::class_name` as a range-pattern table.** Worth doing, and worth
  doing as the field build proposed it — offered rather than applied, because it raises
  that module's floor to v0.3.0. That is a real cost for a library that may want to
  support the previous release, and the decision belongs to whoever maintains its
  compatibility promise, not to whoever wrote the feature.

### The syntax review (2026-08-20) — what remains, and what was declined

From 13 libraries / 117 modules / 15,260 lines, every claim probed against the released
binary. Tier 1 landed in `2d6788f`; the verbs it kept hand-rolling in `64bba1c`; range
patterns in `a0bd498`. What follows is the rest, each with a decision rather than a
wish, because an undecided list is what a later reader has to re-litigate.

**Worth doing, in this order.**

- **Block comments.** Only `#` line comments exist, so every one of 117 module headers
  is a 20-line run of `#`. A lexer change (`lex_trivia` already carries comments as
  trivia) plus `fmt` awareness — `fmt` re-emits comment bytes verbatim and never
  reflows, so a block form is preserved by construction, but its indentation rule needs
  stating. The largest purely cosmetic cost in the corpus.
  **STATUS: landed in v0.3.0** — `#[ … ]#`, nesting, with an unclosed-block hint
  (src/lexer.rs).
- **`group_by(key)` on arrays.** A DataFrame has `group`; an array has `frequencies`
  (counts only) and nothing that groups elements by a computed key, so the corpus folds
  a dict by hand. Distinct from `chunks`/`windows`, which group by POSITION. Returns
  pairs, so it composes with the newly-widened `to_dict`.
- **Dict spread into a record** — `{...d, k: v}` where `d` is a Dict is refused
  ("`...` record update needs a record, got a Dict"), which forced branch-per-field code
  in `llm/request.helix`. A record has fixed known fields and a dict has dynamic keys,
  so this is a real question (what is the field set?), not an oversight — but the
  one-way direction (dict → record, keys must be strings) is answerable.
  **STATUS: landed in v0.3.0** — a Dict spread base is accepted (string keys become
  fields; both engines — see the `RecordUpdate` Dict arm in src/interp.rs / src/vm.rs).
- **Destructuring in `let` and `fn` parameters.** `a, b = expr` works as a STATEMENT
  and lambda parameters destructure tuples, so the gap is narrower than it looks:
  `let [a, b] = …` and `fn f([a, b])`. Medium cost, and the statement form covers most
  of what the corpus wanted.

**Needs a decision before it can be built.**

- **Record dot vs `.get` vs Dict dot.** `r.zz` raises, `r.get("zz")` yields `missing`,
  and `d.a` on a Dict raises "has no field". Three behaviours to hold in mind whenever a
  value might be either. Any change here is a semantics decision (is absence an error or
  a `missing`?), and ADR 0020 already answered it for Dict INDEXING — `d[k]` yields
  `missing`. Aligning dot with that is defensible; making it silent is also how a typo
  stops being caught. Owner's call.
- **Selective import** (`from status import reason`). `import status as st` works. This
  is module-system surface and interacts with the parse-time import-name resolution
  already recorded above as scope-blind.
  **STATUS: shipped** as `import lib.mod.{f, g}` (no new keyword — the brace tail
  mirrors the dotted path; see ROADMAP Phase 7).

**Declined, with the reason, so it is not re-proposed.**

- **`elif`** — `else if` already spells it. A second spelling for one concept is what
  ADR 0003 exists to prevent, and the 162 ladder arms wanted the TABLE (now available as
  range patterns), not a shorter ladder.
- **`fold` as an alias for `reduce`**, **`each`** alongside `map` — same rule. One verb
  per concept; an alias is a second way to say the identical thing.
- **`flat_map`** — `map(…).flatten()` composes and is one obvious way. If it is ever
  worth adding, the reason will be that the intermediate array is a measured cost, and
  then the right fix is fusing the pair, not a third verb.
- **String `+`** — interpolation is the everyday spelling and `concat` now exists for
  the sequence-joining sense. Overloading `+` across numbers and text is the ambiguity
  the current error message already explains well.
- **Ternary `a ? b : c`, pipeline `x |> f`, comparison chaining `1 < 2 < 3`, `xor`** —
  `if/then/else` is an expression, method chaining IS the pipeline, and chained
  comparison already has a good error explaining itself. Each would be a second spelling.
- **Early `return`** — the review did not ask for it and said so; totality is the point
  of `if` requiring `else`, and a `return` reintroduces the statement/expression split
  the language does not have.

**Confirmed non-problems** (the review corrected its own notes; recorded so they are not
re-reported): `{{` and `\{` both escape a brace in interpolation; `//` is integer
division and `/` is float division, deliberately; unary minus works everywhere (the
corpus's 176 `0 - x` were habit); tail calls are optimised and the 20,000 cap only binds
NON-tail recursion — though that error should say "this call is not in tail position",
which is a message worth improving.

### Recorded by the scalars→tensor bridge survey (2026-08-20)

The bridge itself landed. These are the boundary decisions it surfaced and did not
make, each verified across all three engines; the first is the one that most wants
an owner's ruling.

- **DECIDE: which spelling of an activation survives.** `tensor([w1, w2]).exp()`
  works (the tape's method table) while `tensor([1.0, 2.0]).exp()` does not (plain
  tensors have no such method) — nine names: relu, sigmoid, tanh, exp, ln, sqrt,
  sin, cos, abs. The FUNCTION form (`exp(t)`) works on both, so it is already the
  universal spelling and ADR 0003's "one verb per concept" points at dropping the
  nine methods from `autodiff::method`. That is a breaking change for anyone who
  wrote `x.sigmoid()` on a tracked value, so it is a release decision, not a
  drive-by. The alternative — adding all nine to `tensor::method` + `TENSOR_METHODS`
  — widens the surface instead of narrowing it. The asymmetry predates the bridge;
  what the bridge changed is that `tensor([w, …])` is now the natural way to build a
  tracked tensor, so an ordinary program reaches it.
- **`to_array` stays the tape EXIT, deliberately** (it already carried a code comment
  saying so). Making it return tracked scalars was probed and rejected on evidence:
  on the resulting array `contains` flips true→false, `index_of` returns `missing`,
  `unique` stops deduplicating, and `max`/`min`/`std`/`median`/`cumsum` break
  outright — all silently, all at exit 0. It would also change `.sum()`'s ANSWER
  (packed Neumaier 100.0 vs naive tape fold 99.9999999999986 over 1000×0.1), and a
  2-D tracked tensor has no good shape convention (`to_array` flattens row-major).
  The differentiable capability already exists as `range(0, n).map(i => t[i])`, now
  that indexing is on the tape. If a differentiable extraction verb is ever wanted
  it needs its own name and its own ADR.
- **Stacking whole tensors as rows** — `tensor([row1, row2])` where the rows are
  tensors — is refused by BOTH builds today (the plain one says "cannot build a
  tensor from a value of type Tensor"; the tracked one names the tracked case). The
  stack primitive the bridge added would support it directly; widening is a real
  feature and must land in both builds at once, or the legality of an expression
  starts depending on whether a variable is inside it.
- **The receiver-lift at `interp/methods.rs:265` is name-blind**: a plain tensor
  receiver is pulled onto the tape by ANY tracked argument, so
  `tensor(…).solve(variable(…))` reports "a tracked value has no differentiable
  method `solve`" instead of `solve`'s own error. Gate the lift on the tracked
  method table rather than on the presence of a Node argument. Low severity now; it
  grows with every method added to either side.
- **`Node` leaks as a type name** in FEWER error paths after 2026-08-24 (the
  min/max/clamp/hypot/floor/round/sign families now answer differentiability
  errors or differentiate, never "found a value of type Node"), but the generic
  `type_err` fallback can still surface it (`value.rs:669`).
  Display is clean — a tracked value prints as its value everywhere, including in
  interpolation — so a user who meets the word has no way to connect it to anything
  they wrote. Route user-facing mentions through "a tracked value".
- **The real `d/db a**b` pow node** belongs to the same tape-surface batch (v0.2.7
  made a tracked exponent refuse rather than silently answer 0.0).

- **A reduce's INIT must be a literal or the kernel silently never compiles (~21-53×)** —
  the llm-library field report's finding, VERIFIED on HEAD 2026-08-16: identical body,
  identical answer, `reduce(1.0, …)` 59 ms vs `reduce(a0, …)` (a0 a parameter) 3,117 ms
  at 100M. Mechanism: FIVE guards key on `matches!(init, Expr::Float(_))` (jit.rs:1412,
  :1551, :1939; bytecode/comprehensions.rs:699, :883 — the last admits `Int(_)` too).
  The literal-match is a cheap static TYPE oracle, not a value requirement — the compile
  site already `compile_expr`s the init and passes its VALUE on the stack, so the kernel
  is init-value-agnostic. Fix design: dispatch on the RUNTIME init kind at the guard
  (the VM pops init anyway: `Value::Float` → f64 kernel, `Value::Int` → i64, else fall
  back) — the same runtime-representation dispatch the map kernels already use. This
  hits the natural ODE-integrator spelling `reduce(a0, …)`; the field workaround (fold a
  dimensionless factor from a literal, scale after) should not need to exist.
- **[SCOPED 2026-08-16, the LAST live reduce trap]** the init-cliff half of this family
  landed (`18517d2`); what remains is `let`-in-body, and it is NOT eligibility-only:
  `gen_f64_typed`'s `F64Ctx.binders` is an IMMUTABLE borrow (`&HashMap<&str, (Variable,
  NumKind)>`, jit.rs:2031), so the codegen needs scoped binder extension (save/restore
  or a per-Let overlay) before the three analyses (`float_reduce_body_eligible`:1389,
  `f64_range_body_eligible`:1564, `infer_f64_indexed`:1651) can gain Let arms typing the
  local by its init's kind — with the i64 path's rebind guards (no shadowing `pa`/`pb`/
  an index scalar, jit.rs:1318's rules) mirrored. The field's `%`-in-float-body
  corollary belongs to the same widening (the float op set is `+ - *`; the i64 set
  already admits guarded `%`). Field-measured 19-23×; a NOJIT control column is
  mandatory (their `xs[i % 1000]` probe once produced a 1.0× false negative because the
  modulo blocked BOTH arms).
- **`let` in a float reduce body falls off the JIT kernel** (from the physics-library
  field report: ~23× claimed, mechanism CONFIRMED 2026-08-15, magnitude not yet measured
  at honest load). The i64 eligibility paths admit `Expr::Let` (`value_eligible_cap_indexed`
  jit.rs:1318 with the rebind/shadow guards; `gen_value` compiles it), but the mixed/f64
  indexed inference (`infer_f64_indexed`, ~jit.rs:1600-1700) has NO Let arm — `let d =
  x[j] - y[j] in a + d*d` declines to the VM loop while the write-it-twice spelling is
  native. Fix = mirror the i64 Let discipline: an arm in `infer_f64_indexed` typing the
  let-local by its init's `MixT` kind (reject rebinding `pa`/`pb`/an index scalar — the
  same guards as jit.rs:1318), plus the `gen_f64_typed` Let arm binding a typed local.
  Measure the claimed 23× on an idle box FIRST (fail-first), then land with an n-vs-4n
  pin. Until then it stays a documented footgun (AGENTS.md #5's class: silent perf,
  never wrong answers).

- **`helix describe` signature enrichment** (describe-sigs — claim fully current, v0.2.1 output has zero signature keys, verified by walking all JSON keys): probe `builtin_type` (types/signatures.rs:36-817) with `vec![Type::Unknown; k]`, k=0..=5 — sound because `compatible(Unknown,_)=true` (types.rs:100) and every non-arity guard admits Unknown (verified arm-by-arm) — Ok-set = arity; refine returns per-k via palette probes ([Float;k],[String;k],[Array(Float);k],[Unknown;k]). Additive JSON: `params` (new hand-authored data in `BuiltinDef`, 131 entries), `arity` (`null` for the 15 unguarded builtins at signatures.rs:780-795 — never fabricate), `returns` (per-arity; structured record/tuple rendering; `null` for comprehension verbs typed in synth.rs:353-414 / parser desugars, and for all Dict/Net methods — no checker tables). Needs: pub wrapper seam at types.rs:948-949; drift-pin test (every builtin probes to nonempty arity or is arity-null — converts a future Unknown-rejecting guard into a gate failure); decision on `universal_methods` strings→objects (cli.rs:2428-2462 only checks `.as_array()`, but external tooling may index strings — or add a parallel key).
- **string FIX 2 — `Op::AppendStrIntoLocal`**: linearize `\"{acc}{x}\"` folds on VM/JIT (currently ×13–21 per ×4 n, but only 3.4s at 2MB — real, not urgent). New opcode beside ConcatIntoLocal (ops.rs:183), hbc.rs name row (~:582), every exhaustive Op match, byte-for-byte MAX_STRING_LEN error parity (vm.rs:1275-1332, interp.rs:33 = 1 GiB), decline on format-spec holes, write_value fallback for a non-Str first iteration, scan-shares-emit-path test. **Coordinate with the uncommitted comprehensions.rs/jit.rs work.**
- **string FIX 3 — walker append wall**: the walker never got the v0.2.0 fix (256k int appends 6.33s — numerically the pre-fix 6.493s). `concat_in_place`/`insert_in_place` are vm.rs-only callers; the walker's reduce (interp/comprehensions.rs:258, rebinding at :317-333) keeps the Rc shared. Touches the binder save/restore choreography — decline for same-name binders `(a,a)`; own design pass.
- **DataFrame missing-filter semantics**: `where(@v == missing)` → 0 silently; `.is_missing()` rejected in queries; no `drop_missing`. Fix-plan already says 'work or error'. ADR-0001-level query-semantics design. **STATUS: resolved the "work" way** (`.is_missing()` in queries, frame `drop_missing()`; `== missing` stays missing-propagating by design — see the sweep entry above).
- **Printable signature/doc-line metadata**: the new source of truth doc-verb's original ask needs; only after the probe-based describe enrichment settles the shape.
- **`Dict.get(k, default)`** — Record.get already takes a default (methods.rs:634-645), Dict's is strictly 1-arg: an undocumented asymmetry; decide inside the ADR 0004 errors-as-values work.
- **D2 `--explain-jit`** — already deferred to v0.2.2 with a sketch (fix-plan STATUS 5). AGENTS.md documents the cliff meanwhile.
- **AGENTS.md rot pin** — a test that executes its command examples, since footguns 1 and 5 have scheduled fixes and nothing fails today if the file rots.

Recorded by the consolidated 0.4.0 field review response (2026-08-24):

- **`where` clauses** — ADR 0035 ACCEPTED 2026-08-24 (decision delegated by the
  owner) and implemented as the fn-only parser desugar the ADR specifies.
- **Canonical record print order — DECIDED (2026-08-24, delegated): SORT.**
  Records now print with fields in sorted order, aligning the printer with
  `to_json` and with `==` (which always ignored order), so a doc example
  documents the VALUE, not the construction route. Breaking output change,
  shipped under the next minor with a release note.
- **Float `.0` default printing — DECLINED (2026-08-24, delegated), final.** The
  trailing `.0` is what keeps Int and Float distinguishable in printed output —
  load-bearing for a language whose three engines are byte-compared and whose
  numeric tower splits the two types. `{x:g}` is the documented spelling for
  display contexts (CSS), and the docs table's notes say so on the spot.
- **Array `find`** — BUILT AND WITHDRAWN 2026-08-24: Dna owns `find` (motif
  search) and desugars are receiver-blind, so the parser desugar hijacked
  `seq.find("ATG")` (three gate tests + an example caught it). The spelling stays
  `filter(p).first()`; do not re-add without receiver-aware dispatch.
- **`group_by` / `partition`** — still deferred: group_by needs engine
  closure-calling (a reduce-desugar exists but is quadratic on group concat);
  partition's desugar would double-evaluate the predicate. Both need engine work,
  not parser sugar.
- **`helix test --json`** (review §3.5) and the **trap lints** (§3.6) — accepted
  in principle, not yet built.
- **A doc block's own `>>> import` preamble** (§3.7) — turned out to ALREADY
  WORK (2026-08-24): a `>>>` block is a multi-line program, and an import line
  composes with the lines after it. It was only undocumented; comments-and-docs
  now shows it, and a CLI pin keeps it true.
- **The Result shape** (review §1.3) — documented in syntax-and-dx.md: `try`'s
  record IS the shape, success carries `error: missing`. Constructors for a
  user-level Result type would be an ADR.

- **Autodiff surface gaps — CLOSED (2026-08-24).** Everything the 2026-08-16 entry
  listed landed earlier (`.sum()`, `sin`/`cos`, `abs`), and the consolidated 0.4.0
  field review's remainder landed with it: the whole routed elementary family
  (tan/asin/acos/atan/sinh/cosh/log2/log10/cbrt/degrees/radians/erf/normal_cdf/
  normal_pdf), max/min/clamp/hypot (ties-to-first, the relu-kink convention),
  Array `.max()`/`.min()` tracked folds, unary minus on a Node, and UFCS falling
  through for names the tape does not own. `describe` now reports a
  `differentiable` flag kept honest by a unit test. Still refused BY DESIGN, with
  the refusal naming the op: floor/ceil/trunc/round/sign (zero/undefined
  derivative). Still open: the real `d/db a**b` pow node (below).

## SKIP (declined, with why)

- **Missing provenance / changing `missing == missing`** — ADR 0001; Arrow validity bitmap has no payload slot (polars.rs:167/:278 round-trip evaporates it); Value is deliberately 16 bytes; 232 sites. `??` + `.has` + `expect` cover the failure modes.
- **Strict-mode env var** — would be the first semantics-changing HELIX_ variable (all current ones verified semantics-preserving); forks the language, breaks one-program-one-answer.
- **i64 wrap / sum-vs-reduce divergence changes** — deliberate, documented (integer-semantics.md). AGENTS.md documents; revisit only inside errors-as-values.
- **Changing `get`/`[k]` absence defaults** — pinned by ADR 0001/0020 and tests.
- **parse-helps c/d/e/g** — already correct house-pattern hints; churn only.
- **Adding String `concat`** — the review's exact program is foreign-idiom; the `+` error already teaches interpolation/join; one-obvious-way.

## STALE CLAIMS (do not re-litigate)

B2 grouped-missing (fixed: prints `null`) · C1 DNA IUPAC (fixed: S→1.0, NNN→missing) · D1 try-hint (shipped) · assert-free `helix test` ok (now FAILs) · `reduce(\"\", concat)` doesn't compile and no string spelling reproduces 75s/150KB (~5000× off; the real quadratic is array-of-strings on all engines + everything on the walker) · \"no efficient append\" (packed arrays linear since v0.2.0; join linear everywhere) · \"signatures/doc lines are printable from the registry\" (no such metadata exists) · parse-helps c/d/e/g (already good) · \"helix doc doesn't exist\" (a type-listing `helix doc [Type]`/`builtins` shipped in v0.2.1).

## Sequencing note

Items 1–6 are independent of each other and of the in-flight comprehensions/jit work; land in any order (suggested: 1+2 together as the discoverability commit, then 3, 4, 5, 6). Item 7 touches value.rs only — independent of the in-flight files, but run the full gate + SOAK because its blast radius is the shared value layer. Every commit: `scripts/gate.sh < /dev/null`, regression test failing on the v0.2.1 binary first, three-engine assertion where a runtime path changed.