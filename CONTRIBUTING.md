# Contributing to Helix

Thanks for looking. This file is short and specific, because a contributing guide that
describes an imaginary process is worse than none.

## Build and run the gate

```sh
cargo build            # debug
bash scripts/gate.sh   # clippy + the full suite + parity + the whole-tree type-check
```

**Run `scripts/gate.sh`, not `cargo test --release`.** `[profile.release]` uses fat LTO and
one codegen unit, which links Polars, noodles and Cranelift as a single LLVM unit — about
twenty minutes and several gigabytes in one rustc. The `gate` profile keeps `opt-level = 3`
so the differential fuzzers and perf-sensitive tests run at full speed, and drops LTO. No
test's pass/fail depends on the optimization level, so it is a faithful gate.

CI runs the same four things, plus `cargo audit` and a non-default-feature check
(`--features python`, `--features managed`, `--no-default-features` — none of which any other
job builds). All five jobs block.

## The one rule that is not negotiable

**Three engines must agree, byte for byte, on values *and* on error text.**

Helix runs your program on a Cranelift JIT (default), a bytecode VM (`HELIX_NOJIT=1`), and a
tree-walking interpreter (`HELIX_NOVM=1`). The JIT is thousands of lines of code generation
standing between a program and its answer; the other two exist so that generation can be
checked against something simpler. A change that makes them disagree is a bug even when the
new answer looks more correct — fix all three, or fix none.

Four gates enforce it (`tests/corpus/`, `scripts/vmparity.sh`, the executed doc-comment
examples, and `scripts/opfuzz.py`), and one more covers what they cannot:
`scripts/checkall.sh` type-checks every `.helix` in the repository, including the benchmark
programs that need generated fixtures before they will run.

See [docs/execution-engine.md](docs/execution-engine.md) for what the oracle has actually
caught.

## What a good change looks like

- **A test that fails without it.** For anything touching the JIT, the test must also assert
  the JIT *engaged* — `crate::jit::native_call_count() > 0`, or better, a delta across the
  call. A differential test passes trivially if the JIT quietly declined, which means a
  green suite can hide a kernel that never ran.
- **A commit message that says why.** The history here is the design record: what was tried,
  what was measured, what was rejected and on what evidence. "Fix bug" costs the next reader
  the whole investigation again.
- **Numbers that were measured, not remembered.** If a change claims a speedup, say what was
  run, on what input, and against what baseline. Wall-clock on a loaded machine moves by
  ±15% here, and it has manufactured a fake 1.7× before now. Peak RSS is far more stable.
  [`bench/kernels/RESULTS.md`](bench/kernels/RESULTS.md) is the page-fair suite and the only
  benchmark source that should be quoted — `docs/jit-benchmarks.md` is kept for its
  engineering history and carries a banner saying its C baselines were wrong by ~4.4×.
- **No workaround where a fix belongs.** If something cannot be done properly yet, say so in
  the code and record the condition that would unblock it — see `.cargo/audit.toml` for the
  shape.

## Style

`cargo clippy --all-targets -- -D warnings` must be clean; CI enforces it.

`cargo fmt` is deliberately **not** enforced. The comment layout in this codebase is
hand-wrapped and reformatting it would destroy information for no functional gain, so the
CI step reports and moves on. Match the surrounding code rather than the formatter.

## Architecture decisions

Anything that changes the language, the engines, or the toolchain contract belongs in an
ADR under [docs/adr/](docs/adr/). Read the neighbours first — several decisions that look
arbitrary (Euclidean `%` and `//`, `@` for columns, no `;`) have a written reason.

## Reporting security issues

Privately, via [GitHub's advisory form](https://github.com/areeb-h/helix/security/advisories/new).
See [SECURITY.md](SECURITY.md) for what is in scope — notably, a panic on malformed input is
a normal bug here, not a security report, because ADR-0024 already ratchets it in CI.
