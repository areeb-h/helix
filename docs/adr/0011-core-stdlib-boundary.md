# ADR 0011 — Core/stdlib boundary: builtin registry & native namespaces

- **Status:** Partially superseded by [ADR 0017](0017-methods-and-functions.md)
  (the *native-namespace* decision is reversed — domain builtins are now methods on
  data or plain free functions; the **registry** as single source of truth and the
  **small-core** boundary remain in force).
- **Date:** 2026-06-24
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0003 — Collection API](0003-collection-api.md),
  [ADR 0002 — Type system](0002-type-system.md)

## Context

Every domain capability — bioinformatics readers (`read_vcf`, `read_fasta`),
statistics (`t_test`, `linear_regression`), I/O (`read_csv`), formats (`parse_json`),
networking (`http_get`) — had been added as a **flat global builtin**, in the same
namespace as math primitives like `sqrt`. The set had grown to ~50 functions and ~75
methods and grew with every feature. Two concrete problems resulted:

1. **No namespace seam.** Every new domain function welded another name into the global
   grammar. A flat global namespace does not scale: the canonical failure is PHP's
   thousands of flat functions, whose naming became inconsistent enough to need a
   language-wide RFC — which then *abandoned* retrofitting namespaces as too
   backward-incompatible. The lesson is to introduce the seam **early**.
2. **Duplication and drift.** The builtin name list was hand-maintained in the
   interpreter and the type checker (plus a partial third copy), and each receiver
   type's method names appeared in 5–7 places. They had already drifted (`is_missing`
   and the tensor method set differed between the checker and the runtime) — a latent
   "checker says yes, runtime says no such method" class of bug.

A research pass (24/25 claims confirmed against primary sources: the PHP RFC, the Lua
design paper, CPython PEPs, rustc source, mlua) informed the decision.

## Decision

**1. A single builtin/method registry** (`src/registry.rs`). One table is the source
of truth for builtin paths (and purity) and for each receiver type's method names;
`is_missing` is universal and held once. The interpreter, the bytecode VM, the type
checker, and the error-hint sites all derive their name sets from it; a test enforces
that no name is declared twice. This is the registry pattern of Lua's `luaL_Reg` and
CPython's `PyMethodDef`, with rustc's `symbols!{}` uniqueness guarantee as a test.

**2. Compile-time native namespaces** (`src/namespace.rs`). Domain builtins live under
dotted paths (`bio.read_vcf`, `stats.t_test`, `io.read_csv`, `json.parse`, `http.get`).
A rewrite pass — run after module loading, reusing the module loader's
`alias.member`-resolution strategy — turns `bio.read_vcf(args)` into a direct
`Call("bio.read_vcf", args)` that rides the ordinary builtin path; no new opcodes, and
the resolution is static (good for the VM/JIT). Namespaces are predefined (no import,
matching scientist DX) but shadowable by a local of the same name. Compile-time paths
were chosen over first-class runtime namespace values because they fit the existing
static pipeline and keep dispatch monomorphic.

**3. A small global core.** Language primitives stay global: all math (incl. `erf`),
`print`, `range`, `dna`, `tensor`, the array constructors, and the `to_*` conversions.
Everything domain-specific is namespaced. This follows the small-core discipline
(PEP 594's maintenance argument; Rust keeping `rand`/`regex`/`serde` external).

**4. One home for domain logic.** The brief in-language `std/` Helix modules are retired;
their composable helpers are folded into the native `stats.*` / `bio.*` namespaces. A
Helix-source standard library can return once a package manager exists. The module
search-path and selective-import machinery remain as general user-code features.

## Consequences

- The drift bug class is eliminated by construction: one name set, enforced unique.
- Adding a domain function is a registry entry plus a namespace, not another global
  keyword; the grammar stops growing with the domain surface.
- This is a **breaking rename** (`read_vcf` → `bio.read_vcf`, etc.), taken deliberately
  while the user base is ~0; a retired-name hint maps each old name to its new path.
- Deferred: inverting builtin *ownership* so each function's implementation and type
  signature are fn-pointers carried by the registry entry (rather than match arms keyed
  by the same name). The name-set drift — the bug that mattered — is already closed; the
  remaining duplication is ordinary impl-vs-signature separation, addressed by a
  dispatch-agreement test rather than a large mechanical refactor.
