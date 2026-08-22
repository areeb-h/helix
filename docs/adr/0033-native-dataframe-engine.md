# ADR 0033 — A native DataFrame engine: replace polars, staged, with polars as the oracle

- **Status:** **Accepted 2026-08-23; Stage 0 implemented** (commit `5c203dc`) — the
  frozen frame format lives in `src/framefmt.rs` (the module doc IS the spec; its
  first test asserts the spec's example byte-for-byte), `collect_string` left the
  seam, `POLARS_FMT_*` no longer reaches program output, three engines verified
  byte-identical. **Stage 1 implemented** the same day: `src/backend/native/`
  (ten single-purpose files), the full DataHandle surface through the
  interpreter's own scalar kernel per ADR 0034, 12/12 differential tests against
  the polars oracle (the harness caught two real ordering divergences on first
  contact — keep-last row order, right-join column layout — both fixed), the
  dataframes examples byte-identical across engines modulo the decided deltas,
  and the appliance profile now ships WORKING frames at 9.3 MB. First honest
  perf probe (1M rows, CSV-heavy, min of 3): native 156 ms vs polars 77 ms —
  parse-dominated, the Stage 3 item, outputs byte-identical. Stages 2-3 next.
- **Date:** 2026-08-23
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0012](0012-dataframe-backend-seam.md) (the seam that makes this
  possible — "go homegrown later" was its stated purpose), [ADR 0032]
  (0032-appliance-profile.md) (the gate pattern Stage 1 rides), [ADR 0001/0025]
  (missing/ordering doctrine the native engine implements directly),
  [docs/binary-size.md](../binary-size.md) (the unfixable-tail proof).

## Context: what the audit established

- **The seam is airtight and already survived production.** `polars` appears in six
  files; the trait surface is 18 methods + 3 free functions, all shared by both
  engines (no second copy to drift). ADR 0032 shipped "compile with zero backends."
- **Half of backend/polars.rs is armor, and that is the real maintenance cost.**
  The API-churn argument is PROSPECTIVE, not historical — the repo has never paid a
  polars upgrade (pinned 0.54 since the seam landed). What the log actually shows is
  five commits defending against the engine's runtime behavior: nondeterministic
  group order (e079749), join-order coin-flip silently mispairing ~490/500 rows
  (62716f1), a fast-count answering 1 with exit 0 on an unparseable file (cd05369),
  `read_csv(f).where(1)` aborting the process (c243040 — "ADR 0024 was false in
  production"), parity divergences (71b94bb). Helix's determinism and
  missing-propagation doctrines run against polars' grain; an eager,
  single-threaded-per-shard native engine makes that whole failure class
  structurally impossible.
- **The tail is unfixable on 0.54.4.** object_store -> reqwest -> hyper -> tokio
  enters via `polars-error` unconditionally (~35-39 MB of the default binary, 192
  lock crates). The upstream fix is post-0.54, purchasable only by paying the
  unstable-API upgrade. There is no free "keep" branch: pay churn to shrink the
  tail, or pay the swap to delete it.
- **Nothing pins polars' table text.** `shape: (` appears in zero tests, examples,
  docs, or corpus fixtures; the interactive rich-print path is ALREADY Helix-owned
  (render.rs builds its own table from seam methods). The scariest surface —
  matching polars' Display byte-for-byte — is optional.
- **The oracle harness already runs in CI.** 143 corpus fixtures pinned
  engine-identical, dfcheck.sh (built to catch row-order nondeterminism), vmparity,
  two differential fuzzers. The `.expected` files ARE frozen polars output. Both
  backends can coexist in a dev binary (cross-backend join/vstack already errors
  cleanly via `as_any`), so side-by-side property tests need no seam changes.

## Decision (proposed): replace-staged

Polars becomes the development oracle for its own replacement. Each stage ships
alone; the default flip is one release with polars retained behind a feature for a
rollback window.

**Stage 0 — annex piped frame rendering (before any engine code).** Extend
render.rs's engine-agnostic table path to non-TTY `print(df)` and interpolation,
with a frozen Helix format spec (Helix float formatting, the `missing` token, fixed
truncation, no `POLARS_FMT_*` env sensitivity — today's env porosity violates
render.rs's own byte-identity doctrine). A deliberate, versioned, release-noted
format change, decoupled from the engine change, landed while polars is still the
engine.

**Stage 1 — `NativeFrame` as the appliance profile's engine (pure addition).**
`enum NativeCol {I64, F64, Str, Bool}` + validity masks; `build_frame`; every
in-memory trait method (filter/select/with_columns/sort/join/group_agg/unique_by/
head/vstack-with-eager-dtype-check/cache=identity/...); full ColExpr eval — with
the `%` truncated-vs-euclid and `/0` error-vs-null policies decided in writing
FIRST; `read_csv` on the csv crate with a documented inference policy; `write_csv`;
`collect_string` = the Stage 0 renderer; parquet answers the ADR 0032 gated error.
Feature `native-df`, composable into `appliance` (~14-15 MB with frames). Appliance
has no frames today, so the regression surface is zero. Acceptance gate: full
corpus + dfcheck + fuzzers against the polars-frozen `.expected` files, plus a
dual-backend binary running side-by-side property comparisons; every divergence
fixed or written down as a decided policy delta.

**Stage 2** — apache `parquet` (no arrow glue) for the four dtypes +
foreign-dtype-to-string fallback + footer-metadata count. **Stage 3** — parallel
CSV parse (rayon chunks over memchr record splits) + filter kernels, measured
against the 113 MB crosslang anchor under the house measurement discipline.
**Stage 4** — flip the default; polars retreats to the `python` feature (the
pyo3-polars Arrow bridge keeps it), then out entirely if the bridge moves to plain
Arrow interchange.

## Honest costs, stated up front

- **4-6k LOC and a multi-week campaign** (the audits' 2.2-3.6k LOC is the code; the
  type-coercion matrix, CSV-inference policy corners, and differential triage are
  the schedule). Still an order of magnitude below polars-parquet alone (48k LOC),
  and every corner has an executable oracle.
- **Big-CSV ingest gets slower first** (csv crate is single-threaded; expect 3-5x
  on the 113 MB anchor until Stage 3), filter kernels 3-4x (measured in dfbench).
  Offsetting wins the same users feel: today every `.column()` RE-EXECUTES the
  whole lazy plan unless the user knows to `.cache()`; the honesty-fixed `count()`
  costs 280 ms per call on the anchor — eager makes both free, and group_agg
  measured 4-25x faster on dense keys. Out-of-core frames are lost — the flagship
  bio path never had them (five of seven genomics readers already build eager
  ColData).
- **Stage 0 is a real (minor) breaking change** for external scripts that parse
  piped table text; released with notes, not slipped in.

## What flips this back to keep-polars

1. Stage 1's differential campaign exceeds ~25-30 distinct POLICY decisions (not
   bugs) — the bounded-semantics premise fails, and ADR 0012's insurance was the
   final answer.
2. A polars release with a stability commitment AND an optional object_store that
   puts the default binary under ~25 MB — re-measure before Stage 2.
3. After Stage 3, `read_csv` + filter on the anchor is still >2x slower than
   polars under the house measurement discipline — the hybrid (native-df in
   appliance only) becomes terminal instead.
4. Zero-copy Helix-to-Python frames become a headline feature — pyo3-polars' Arrow
   path keeps polars in the builds that matter (it survives behind `python`).
