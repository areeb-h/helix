# ADR 0018 — Reproducible random numbers (seeded, pure, hand-rolled)

- **Status:** Accepted (implemented)
- **Date:** 2026-06-27
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0017](0017-methods-and-functions.md) (functions vs methods),
  [ADR 0001](0001-missing-data.md) (purity/immutability)

## Context

Helix had no RNG — no `random`/`uniform`/`randn` and no sampler — only the
distribution *functions* `normal_cdf`/`normal_pdf`. Real work (simulations, noise,
bootstrapping, train/test splits, weight init) needs random draws. A userland
`fract(sin(x)*k)` hash works as a probe but has lattice artifacts and seed-to-seed
correlation that can silently bias experiments.

## Decision

Add a **hand-rolled, dependency-free SplitMix64** generator (`src/rng.rs`), exposed
in the ADR-0017 shape:

- **Free functions** (constructors): `random(n, seed)` → `n` uniforms in `[0,1)`;
  `randn(n, seed)` → `n` standard normals (Box–Muller); `random_int(n, lo, hi, seed)`
  → `n` integers in `[lo, hi)`.
- **Array methods** (data verbs): `xs.shuffle(seed)` (Fisher–Yates permutation),
  `xs.sample(k, seed)` (k without replacement), `xs.choice(seed)` (one element).

Three properties make it the right fit for Helix:

1. **Reproducible by construction.** Every draw is a pure function of `(seed,
   index)` — same seed → same numbers, forever. A hand-rolled, pinned algorithm
   (not a crate that could change across versions) guarantees that reproducibility
   survives upgrades. Seeds are **required** — there is no hidden global state, in
   keeping with Helix's purity.
2. **Fits the immutable, map-over-`range` model.** No mutable generator object to
   thread; `random(n, seed)` indexes a stream by element, exactly like `range(n).map`.
   Because the generators are pure, they are also safely **memoizable**.
3. **Decorrelated streams.** Uniform / normal / int / shuffle / sample / choice each
   salt the seed into a distinct stream, so `random(n, s)` and `randn(n, s)` (or a
   `shuffle(s)`) drawn from the same seed never line up.

No external crate (`rand`) is taken — RNG is a textbook algorithm, ~15 lines, and a
pinned implementation gives stronger reproducibility than a versioned dependency
(matching ADR 0015's "delegate parsing, hand-roll the algorithm" stance).

## Consequences

- Quality: SplitMix64's finalizer passes the usual statistical smell tests
  (uniform mean ≈ 0.5, normal mean ≈ 0 / std ≈ 1, decorrelated streams) — verified
  in unit tests over 100k draws — and is vastly better than a sin-hash for sampling.
- Caps: `n` is bounded by the shared 100M element cap, so a stray huge count errors
  cleanly instead of OOM-ing.
- Non-goals (future, if needed): a `Rng` seed-value to thread for sub-streams,
  other distributions (poisson/binomial/beta), entropy-seeded non-reproducible mode.
  The counter-based design extends to all of these without breaking the API.

## Alternatives considered

- **The `rand` crate:** rejected — a versioned dependency can change its algorithm
  (breaking reproducibility), and it's overkill for what a pinned SplitMix64 covers.
- **A stateful generator object (`gen.next()`):** rejected — mutable state fights
  Helix's immutability; the counter-based pure form is cleaner and parallel-safe.
- **Keep it in userland (a `rng.helix` sin-hash):** rejected — statistically weak,
  and a language this clearly needs RNG should provide a correct one.
