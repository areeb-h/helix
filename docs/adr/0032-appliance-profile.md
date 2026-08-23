# ADR 0032 — The appliance profile: a small binary without a smaller language

- **Status:** **Accepted — steps 1+2+4 implemented 2026-08-22/23** (step 4 = the jit gate,
  commit `eb30a33`: appliance **8.6 MB at the shipped release profile** (was 51.8 default; 13.4 -> 9.1 MB gate-stripped); tensor gate
  deliberately never; step 3's re-measure now reads: the next lever is frames WITHOUT
  polars — see [ADR 0033](0033-native-dataframe-engine.md))
  <!-- original: --> — the `dataframes` and `bio`
  gates landed (commit `1e0d56e`): appliance binary **12.7 MB at the
  shipped release profile** (vs 51.8 MB; 13.4 vs 75.7 MB at the gate profile),
  serving at full speed, all five feature configs
  clippy/check clean, default build unchanged. Steps 3-5 (re-measure, then jit and
  tensor only if the number still justifies the surgery) remain open. The
  `dtype-full` question that prompted this is settled and recorded as a non-decision
  below.
- **Date:** 2026-08-22
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0012](0012-dataframe-backend-seam.md) (the backend seam this
  cashes in), [ADR 0021](0021-capabilities.md) (gates hit bodies, never names),
  [ADR 0024](0024-total-runtime.md) (what killed the dtype-trim option),
  [docs/binary-size.md](../binary-size.md) (the tail this sheds).

## Context

The release binary is 51.8 MB, ~80% of it Polars and the `object_store -> reqwest ->
hyper -> tokio` tail it drags in unconditionally. On the machines the old-hardware
work targets (512 MB - 2 GB, slow disks), binary pages are RSS: the 12.7 MB baseline
breaks down as ~9 MB of mapped binary against ~4 MB of heap, so size IS memory here.
docs/binary-size.md concluded the only fixes were forking Polars or rewriting the
backend — but it predates ADR 0012's seam being finished, and the audit shows the
seam is airtight: **no `polars::` type escapes `src/backend/polars.rs`; 7 reference
sites exist outside it, across 3 files, 4 of them already behind `python`.**
`Value::DataFrame` holds a trait object (`Rc<dyn DataHandle>`), so a build with zero
backends still compiles — nothing downstream needs a cfg.

## Decision (proposed)

Feature-gate the heavy backends behind the house pattern the `http` feature already
proves: **gate the body, never the name.** The registry, type checker, capability
table, `describe`, and completions stay identical in every build; a gated builtin
runs into a clean runtime error naming the feature to rebuild with. The flagship
identity is a language-surface property and survives every profile untouched.

```toml
default   = ["http", "mimalloc", "dataframes", "bio", "jit", "tensor"]  # unchanged product
dataframes = []                # polars behind the ADR 0012 seam
bio        = ["dataframes"]    # noodles x6 + needletail; 5 of 7 readers return Df
jit        = []                # the cranelift half of src/jit
tensor     = []                # faer det/solve/matmul
appliance  = ["http", "mimalloc"]   # the small-server profile, opt-in, CI-checked
```

Sequencing by return-on-surgery, each step shippable alone:

1. **`dataframes`** — Small (7 sites + 4 builtin arms + ~35 test cfgs) for ~80% of
   the binary. Ship, measure, update binary-size.md.
2. **`bio`** — Small-Medium; requires hoisting `open_maybe_gzip`/`widen_f32` out of
   `vcf.rs` first (`bed.rs`/`gff.rs` import them but use no noodles) — do the hoist
   as its own ungated commit. `bio` implies `dataframes`.
3. **STOP and re-measure.** After 1+2 the binary is plausibly single-digit MB.
4. **`jit`** — Large, only if step 3's number justifies 15 cranelift crates:
   `src/jit.rs` (7,371 lines) interleaves cranelift codegen with cranelift-free
   eligibility analysis that the bytecode compiler calls at ~40 sites, so it must
   first split into `jit/analysis.rs` (ungated) + `jit/codegen.rs` (gated) — an
   ungated refactor commit, then a cfg commit. The runtime path is already free
   (`HELIX_NOJIT` -> `jit = None`; `jit/ffi.rs` has zero cranelift). ~60
   `native_call_count` engagement assertions stay honest by keeping `jit` in
   `appliance` OR gating those tests — decide at implementation.
5. **`tensor`** — last or never: faer's accumulation order differs from ndarray's by
   ~1e-13, so a fallback forks float output across the feature matrix — every parity
   and determinism oracle acquires a feature axis, for 2 crates.

Never gate rayon (hot-path, polars pulls it anyway).

CI: extend the existing `features` job to `--all-targets` (bare `cargo check` skips
`#[cfg(test)]` modules — 578 KB of them), add per-feature check cells, clippy
`-D warnings` on the appliance profile, `cargo test` on appliance only, and a size
ratchet asserting the stripped appliance binary under a committed threshold — without
it one stray `use crate::backend::polars::` re-links 27 crates silently. Mirror
whatever lands into `scripts/gate.sh`.

## Non-decision, settled: `dtype-full` stays

Trimming it looked like free size and is neither. The bridge's contract
(`anyvalue_to_value`, backend/polars.rs) is TOTAL: any dtype outside the mapped set
falls back to its string form. With `dtype-full` off, polars' own schema layer
panics on Decimal/Categorical/Struct/FixedSizeList parquet columns
(`field.rs:310`), and under `panic = "abort"` that is an uncatchable exit 134
leaking a cargo-registry path — the exact ADR 0024 falsification the repo already
documents once (backend/mod.rs). The fixture suite would stay green (every parquet
in the tree is Helix-written; none carries an exotic dtype), making it a silent
product regression. And the size win is misdirected anyway: the tail enters via
`polars-error`, not the dtype kernels. Keep `dtype-full`; the lever is the
`dataframes` gate above.

## Consequences

- `cargo install helix` stays the flagship. The appliance build is a named, opt-in
  profile whose binary still *knows* the whole language — `describe read_vcf` works;
  running it says what to rebuild with.
- Two audit quirks recorded for the dx-plan, not this ADR: Categorical values
  stringify WITH literal quotes (`"\"female\""` vs a String column's `female`), and
  `UInt64 > i64::MAX` wraps negative in the bridge.
