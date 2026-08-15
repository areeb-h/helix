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
- DataFrame missing-filter semantics (live footgun 1): `where(@v == missing)` silently returns 0 rows, `.is_missing()` unsupported in queries, no `drop_missing` — the fix plan already slates 'work or error'. This is query-semantics design work under ADR 0001, not a patch.
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
- **DataFrame missing-filter semantics**: `where(@v == missing)` → 0 silently; `.is_missing()` rejected in queries; no `drop_missing`. Fix-plan already says 'work or error'. ADR-0001-level query-semantics design.
- **Printable signature/doc-line metadata**: the new source of truth doc-verb's original ask needs; only after the probe-based describe enrichment settles the shape.
- **`Dict.get(k, default)`** — Record.get already takes a default (methods.rs:634-645), Dict's is strictly 1-arg: an undocumented asymmetry; decide inside the ADR 0004 errors-as-values work.
- **D2 `--explain-jit`** — already deferred to v0.2.2 with a sketch (fix-plan STATUS 5). AGENTS.md documents the cliff meanwhile.
- **AGENTS.md rot pin** — a test that executes its command examples, since footguns 1 and 5 have scheduled fixes and nothing fails today if the file rots.

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