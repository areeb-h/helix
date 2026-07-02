# ADR 0019 — Modules: private-by-default `export`, definitions-only imports

- **Status:** Accepted (implemented)
- **Date:** 2026-06-29
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0013 — Package manager](0013-package-manager.md),
  [ADR 0016 — Build and packaging](0016-build-and-packaging.md)

## Context

The loader (`import a.b.c` → `a/b/c.helix`, deduped by canonical path, cycles rejected,
namespaced by AST rewrite) was structurally sound on **security of resolution** — import
segments must be identifiers, so `import ..etc.passwd` / `import lib/util` are *parse*
errors and no import can escape the project tree — but it had three gaps once files were
split into a real project (the trigger: a downstream project reorganizing ~70 files into
themed subdirectories and asking how sharing/exports/entry-points should work):

1. **No encapsulation.** Every top-level name was public: `util._secret` and
   `util._private(…)` were reachable from any importer. Refactoring a module's internals
   could silently break consumers; nothing was a private implementation detail.
2. **Import executed code.** Importing a module ran its top-level *statements*, so a
   library with a stray `print(...)` (or any side effect) executed merely by being
   imported — the root cause of supply-chain "install runs code" attacks, import-order
   bugs, and the `if __name__ == "__main__"` wart.
3. **Misleading import errors.** `import lib.util.{nope}` succeeded silently; the failure
   surfaced later at the *use* site as a bare "not defined", never naming the module.

## Decision

Adopt the **Rust / TypeScript / ES-modules** consensus, adapted to Helix's script model.

### 1. Private-by-default, explicit `export`
A top-level definition in a module is private unless prefixed with `export`:

```helix
export fn area(w, h) = w * h     # public API
export PI = 3.14159              # public constant
fn _clamp(x) = …                 # private helper — not reachable cross-module
```

`export` is a **contextual** keyword (only special immediately before `fn` / a binding),
so it stays usable as an ordinary identifier everywhere else. A private name is a hard
boundary: it can be neither selectively imported (`import m.{_clamp}`) nor reached by
qualified access (`m._clamp`) — explicitly **not** Julia's leaky model where
`Module.secret` always works. The error names the module and points at the fix.

Chosen over the alternatives: Go's capitalization-as-visibility couples case to meaning
(a non-starter for bio/maths naming — gene symbols, `pValue`, single-letter vars);
Python's `_`-convention isn't enforced (no real encapsulation); Swift's
private/internal/public ladder is more knobs than a small language needs. One keyword,
two states.

### 2. Definitions-only modules (import never runs code)
A **module** (a file loaded via `import`) may contain only definitions — `export`/private
functions, global bindings, and `import`s. A bare top-level *expression statement* (a
stray `print(...)`, any side effect) is an error in a module. Side effects live only in
the **entry** file, which runs top-to-bottom (Helix stays a scripting language). This
makes import *inert*: loading or inspecting a dependency can never trigger I/O or
execution — the single highest-leverage supply-chain decision — and removes import-order
bugs and the `__main__` guard entirely, because the guard is now structural.

The **entry/module split is the spine of both decisions**: a file is either an entry
script (runs, may have side effects, exports nothing) or a module (loaded,
definitions-only, has an `export` surface). `export` is therefore meaningful only in
imported files; a single-file script pays zero tax (no imports → visibility is moot).

### 3. Clear import-time errors
Selective imports validate against the dependency's exports at load time:
`import m.{nope}` fails *at the import*, naming `m` and listing it as not exported.

## Consequences

- **Encapsulation is real and composes:** definitions-only means an import can't *run*
  internals; private-by-default means it can't *name* them. Together: Rust-grade module
  isolation, the thing Julia lacks.
- **Packaging/tooling:** the `export` surface is the single source of truth for a
  bundler (tree-shaking), `helix doc`, autocomplete, and semver ("non-`export` items may
  change in a patch").
- **Enforcement lives in the loader** (`module.rs`) — visibility is checked during the
  existing AST-rewrite/namespacing pass, downstream of nothing. The type checker and both
  engines run on the already-flattened, mangled program and are untouched, so the
  differential oracle / vmparity are unaffected by construction.
- **Migration:** the committed multi-file example (`geometry.helix`) and the cross-module
  test fixtures gain `export` on their public names; this is a deliberate breaking change,
  taken pre-1.0 with ~0 users (the right window).

### Follow-up fixed — named args & defaults survive qualification

Named arguments and default parameters ([ROADMAP](../ROADMAP.md) Phase 1) are resolved
at **parse time** against a function's recorded signature. But a *qualified* call into
an imported module — `dep.f(x, open: -10)` — was rewritten by the loader's namespacing
pass into a flat mangled call **before** named-arg resolution ran, so the resolver no
longer recognised `dep.f` as the function `f` and couldn't place the named argument or
fill defaults: qualified calls silently only supported positional arguments. Fixed so
the signature is carried through the rewrite and resolution happens against the
qualified target — `dep.f(x, name: y)` and defaulted parameters now work identically
whether the callee is local or imported (commit `795782e`). This keeps the "named args
are table-stakes DX" promise from being quietly broken the moment code is split into
modules.

### Known limitation (future work)
Module-level globals still initialize **eagerly** in dependency order, so a binding whose
initializer has a side effect (`TABLE = read_csv(...)`) *does* run at import. The
definitions-only rule bans stray side-effect *statements*, not side-effecting
*initializers*. The clean fix is **lazy module bindings** (compute on first access, in the
consumer's context) — deferred; it needs the evaluator to thunk globals. Until then, keep
expensive/effectful setup behind an `export fn` the consumer calls.
