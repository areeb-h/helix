# ADR 0049 — The sample estimate is the default spread

- **Status:** **Accepted & implemented 2026-09-07** (0.10.0-to-be). The decision is the user's,
  on the field build's population-versus-sample question (`docs/dx-plan.md`, "OPEN, a design
  question").
- **Decision:** `std()`, `var()` and `cov(ys)` divide by n − 1 — the sample estimate, Bessel's
  correction — and so does everything built on the same spread: `summary().std`, `zscores()`,
  `normalize()`, `standard_error()`, `coefficient_of_variation()`. `std(0)`, `var(0)` and
  `cov(ys, 0)` are the population's. A spread over fewer than `ddof + 1` values — one value,
  under the default — is `missing`, never an error.

## Context

Two defaults disagreed inside one language: the array verbs divided by n (NumPy's default)
while the grouped frame `std(@col)` divided by n − 1 on both backends (pandas', R's and
Excel's default), so `df.group(@k).std(@v)` and `df.column("v").std()` answered different
numbers for the same column. The `ddof` argument (0.9.1) let a program ask for either on
arrays, which made the disagreement visible rather than resolving it. `stats.rs` also claimed
the population default kept the array verbs in agreement with the group aggregations, which
had not been true.

## Consequences

- One `std`. The grouped frame verb is unchanged; the array verbs, the descriptive record
  and the z-score family move to it. Every value that moves is a number, so
  `tests/compat/MIGRATIONS.md` lists the spellings and the corpus golden that changed.
- `missing` for an undefined spread is the rule a one-row group already followed: no spread
  is an absent datum (ADR 0036 policy 3's `missing`), not a computation failure and not a
  mistake in the program. The old refusal ("needs more than 1 value(s)") is gone; `ddof`
  itself is still refused when negative, non-integer, or given twice.
- `normalize()` and `zscores()` of a single value keep their zero-spread refusal: there is
  nothing to rescale by.
- `stats::population_variance` / `population_std` keep the population formulas under their
  honest names for the callers that mean them; `std_ddof` / `variance_ddof` carry the default.
- Inferential statistics were already sample-based (`t_test` uses `sample_variance`) and
  are untouched; so is `correlation`, whose ddof cancels.

## What the differential campaign compares

`the_sample_estimate_is_the_default_on_every_engine` pins every spelling on the three engines;
`var_std_ddof_sample_option` and `std_and_var_take_a_ddof` pin the estimator and the `missing`
rule; the corpus program `df_records_schema` pins `cov`'s default on both frame backends.
