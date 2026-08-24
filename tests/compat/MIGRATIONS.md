# Recorded behavior changes

Every entry here is a program whose output **intentionally** changed after a baseline
was captured, with the reason. `compat_baselines_hold` reads this file: a program
listed here is a known change and no longer fails the gate; a program not listed here
that drifts is a regression.

Format — the program name in backticks, then what changed and why:

```
- `examples__dataframes__dataframes` — v0.6.0, ADR 0033 Stage 4: the native engine
  became the default, so `@resting_hr / (@age / 10)` now uses the language's true
  division (`4.1`) instead of the polars backend's integer division (`4`).
```

The example above is illustrative, not an entry. There are none yet.

## v0.5.1 → (unreleased)

_(none yet)_
