# ADR 0021 — Capability sandbox: deny-by-default authority

- **Status:** Proposed (phase 1 in progress)
- **Date:** 2026-06-30
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0010 — Networking, privacy, security](0010-networking-privacy-security.md),
  [ADR 0013 — Package manager](0013-package-manager.md),
  [ADR 0016 — Build and packaging](0016-build-and-packaging.md),
  [ADR 0019 — Module system](0019-module-system.md)

## Context

Helix has a strong *integrity* story and no *authority* story. `helix.lock` is
content-addressed and sha256-pinned (ADR 0013), so you get exactly the bytes you
reviewed — but any `.helix` program run with a plain `helix run` has the **full ambient
authority of the user account**: `read_text`/`write_to`/`read_dir` on any path,
`http_get` any URL, `listen` on any port. There is no `--allow-fs`, no `--allow-net`, no
capability block in the manifest.

That is fine while you wrote every line. It stops being fine at exactly the moment the
project succeeds along its stated roadmap: a **self-improving "mind" that generates and
executes its own Helix code**, and a **package ecosystem** where third-party code runs in
your interpreter. At that point "every program has full fs/net authority" is the
containment boundary you wish you had built earlier.

The registry already records `pure: false` on every effectful builtin — the exact seam a
capability system bolts onto. This ADR decides the model. It was preceded by a four-track
prior-art study (Deno's permission model; the object-capability literature; OS sandbox
primitives; and the supply-chain / agentic-execution threat record); sources are linked at
the end.

### Why integrity (which we have) does not give authority (which we don't)

A content-pinned lockfile answers *"did I get the bytes I expected?"*. It cannot answer
*"should this code read `~/.ssh` and open a socket?"*. A pinned hash of a malicious-from-day-one
package (typosquat, or the `event-stream`→`flatmap-stream` transitive backdoor) reproduces
the *exact malicious bytes* faithfully — and runs them with full authority. Integrity and
authority are orthogonal axes. Helix has the first; this ADR adds the second.

### The agentic driver

A code-generating, code-executing mind is the textbook host for the **"lethal trifecta"**
(private-data access + untrusted content + an exfiltration channel). You cannot filter the
prompts — the agent writes the code. The only structural mitigation is to **sandbox the
executor**: deny the exfiltration channel (net) and the private data (fs/env) by default,
granting only what is declared. This makes deny-by-default non-optional for the executor.

## Decision

**Authority is a pinned, attenuable capability — denied by default, granted declaratively
in the manifest, enforced at one chokepoint, and carried per-evaluation rather than
globally.**

### 1. Effect categories (refine `pure: false`)

The boolean `pure` flag is the right hook but too coarse — it lumps harmless effects
(`print`, `emit`, `sleep`, `clock_monotonic`, `aes_keygen`) with real authority
(`read_text`, `http_get`, `listen`). Each builtin/method gains an **effect category**:

```
Pure | Output | Clock | Rand | FsRead | FsWrite | Net | Process | Env
```

`Output` and `Clock`/`Rand` are **never gated** — they are not authority. The gated set is
`FsRead`, `FsWrite`, `Net`, `Process`, `Env`. (`pure` stays, for memoization; effect is
the orthogonal, finer axis.)

### 2. One declarative grant surface — the manifest, not flags

```toml
[capabilities]
fs.read  = ["./kb", "./data"]
fs.write = ["./out"]
net      = ["api.example.com:443"]   # or "none" | "all"
# process / env: absent  ⇒  denied
```

The manifest is the **durable, content-pinned source of truth**: granted capabilities are
recorded per package in `helix.lock` alongside the sha256, so a grant is *keyed to the
exact code it was granted to* — integrity and authority pinned together. This is the thing
Deno never built: a one-time approval that becomes a tamper-evident, content-addressed
grant (re-prompt only when the hash changes).

CLI flags (`--allow-fs=./kb`, `--deny-net`) exist **only as per-run overrides**, with
`deny > allow` precedence (the Deno 2.x lesson). The manifest, not the flag set, is the
canonical grant.

### 3. No interactive prompts — fail closed

Helix is script / server / CI-shaped. Deno's prompt-on-access model lost to **prompt
fatigue** (users reflexively approve; unattended runs hang). Helix grants via
manifest-or-flag; anything ungated is **denied, not prompted**. This kills prompt fatigue
and keeps "one obvious way."

### 4. One enforcement chokepoint, `cap-std`-backed

Every authority-bearing builtin (`call_builtin`) and method (`write_to`/`append_to`, the
`Conn` verbs `accept`/`respond`/`sse`/`send`) consults a single `Authority` context before
acting — the object-capability literature's requirement that authority not be ambient.
Filesystem and network scoping is done with **[`cap-std`](https://github.com/bytecodealliance/cap-std)**
(the Bytecode Alliance crate behind Wasmtime/WASI): a granted fs root becomes a
`cap_std::fs::Dir` handle resolved with kernel-atomic `openat2(RESOLVE_BENEATH)`, so
symlink / `..` / TOCTOU escapes are rejected **by the kernel**, not by a string check.
Hand-rolled `realpath`-then-`open` is a known-broken anti-pattern (symlink-swap, parent
TOCTOU) and is explicitly forbidden. Net is a host:port allowlist checked before the socket
is constructed. On Linux, optional `landlock` + `seccomp` add a defense-in-depth kernel
backstop.

### 5. Per-evaluation attenuation (the agentic seam)

The `Authority` is an immutable value the parent can only **narrow, never widen**. When the
mind runs self-generated code, it hands that code a *strictly smaller* capability set
derived from its own. Generated code can never exceed — nor re-widen — its parent's
authority. This is delegation-by-value at evaluation granularity (the Austral
"acquire-narrow-then-surrender" pattern), and it is the seam where Helix can later expose
real scoped `Dir`/`Socket` *handle values* for genuine object-capability passing.

### 6. `helix build` bakes the grant as a hard ceiling

The standalone-exe overlay (ADR 0016; `HLXBND01` trailer) grows a capability section
(`HLXBND02`): the resolved, manifest-declared grant is baked into the binary as a **ceiling
the running program can never exceed**, with no prompts — Deno-compile's one unambiguous
win. This is also what lets a future `sealed_mind` be *honestly* sealed: the executor's
authority is fixed at build time.

### 7. Default-deny, shipped via audit → warn → enforce

Default-**deny** is correct, but the rollout is staged so nothing breaks the day it lands
(AppArmor's complain-mode playbook):

1. **Audit (ship first):** the model is live but **log-only** — every gated access by a
   program / dependency / generated snippet emits a "would-be-denied" record. Nothing
   breaks; you harvest the real capability footprint of the stdlib, the package set, and
   the mind's output.
2. **Declare-and-warn:** undeclared access warns loudly; legitimate needs are grandfathered
   into manifests (and pinned into `helix.lock`).
3. **Enforce:** default-deny; undeclared access is a hard error.

The **agent executor is enforced from day one regardless** — generated code never runs in
audit-only mode.

## Security positioning (honest, threat-model-specific)

This is recorded so we never overclaim.

- **vs Python — strictly safer (authority axis).** Python has *no* capability model: an
  imported module or a `pip install`'s `setup.py` runs with full ambient authority, and
  `RestrictedPython` is a leaky bolt-on. Default-deny Helix clears this bar by construction.
- **vs Rust — safer on the axis Rust ignores, peer on the axis Rust owns.** Rust's safety
  is *memory safety*; it has **zero authority confinement** — any safe Rust can read
  `~/.ssh`, open sockets, or `Command::new`, and `build.rs` / proc-macros run arbitrary code
  with full authority *at compile time*. Helix is *memory-safe* by inheritance (programs run
  interpreted on a memory-safe Rust host), **and** authority-confined by default. The
  defensible claim is the **combination — "memory-safe AND authority-confined by default" —
  which neither stock Python nor stock Rust offers.**
- **Two load-bearing caveats.** (a) The guarantee confines Helix *programs*, not the engine
  itself. (b) It holds **only while Helix has no un-gated FFI**: `cap-std` gates I/O, not
  native code, so the day Helix gains FFI it becomes the loud, never-scoped, opt-in tier.
  Helix has **no FFI builtin today** — which is precisely why the claim is true now, and a
  constraint to defend deliberately.

## Alternatives considered

- **Global permission flags as the end state (Deno-style).** Rejected as the *terminus*:
  even scoped, a process-global `--allow-read` is ambient authority shared by every module,
  still confused-deputy-prone, and Deno's own `--allow-run`→`--allow-all` collapse shows the
  category-bluntness failure mode. Flags survive only as thin per-run overrides of the
  manifest.
- **True object-capabilities now** (no global builtins; a root capability injected at the
  entrypoint and delegated linearly, à la Austral / WASI preview 2). Rejected for v1: it
  requires rewriting the stdlib around explicit capability-passing, violating "one obvious
  way" and the no-disruption constraint. Kept as the **deliberate direction** — the
  per-evaluation `Authority` context and future scoped handles are the staged path to it.
- **Userland `realpath`-then-`open` path checks.** Rejected as unsafe (TOCTOU, symlink
  swap, hardlinks). `cap-std` / kernel-atomic resolution only.
- **OS sandbox delegation as the primary mechanism** (pledge/unveil, Landlock, Seatbelt).
  Rejected as the *portable* primitive: pledge/unveil is OpenBSD-only, Landlock is
  Linux-5.13+, Seatbelt is deprecated on macOS. `cap-std` is the portable in-process
  primitive; OS sandboxes are optional Linux-only hardening.

## Consequences

- A young language (~0 users) spends its one-time window to make least-authority the
  *default*, before an ecosystem of ambient-authority-assuming packages exists.
- A real, non-marketing differentiator: memory-safe **and** authority-confined by default.
- Cost is staged and front-loaded onto the engine, not the user: phase 1 (effect categories
  + `Authority` context + manifest parsing + audit logging + coarse `net=none|all`) is
  **non-breaking** — every existing program keeps running, now with an audit trail. The
  larger, separable work is `cap-std` path-scoping (rerouting `read_text`/`write_to`/
  `read_csv`/`read_dir` through `Dir` handles), then enforce-mode and bundle-baking.
- A standing constraint: **keep FFI gated** (or absent) or the headline guarantee acquires
  a hole.

## Sources

- Deno permissions: <https://docs.deno.com/runtime/fundamentals/security/>,
  <https://deno.com/blog/v2.5>, <https://docs.deno.com/runtime/reference/cli/compile/>
- Object-capabilities: Miller/Yee/Shapiro, *Capability Myths Demolished*
  <https://classpages.cselabs.umn.edu/Fall-2021/csci5271/papers/SRL2003-02.pdf>;
  Austral capabilities <https://borretti.me/article/how-capabilities-work-austral>;
  WASI capability model <http://www.chikuwa.it/blog/2023/capability/>
- OS primitives: pledge/unveil <https://man.openbsd.org/unveil.2>; Landlock
  <https://docs.kernel.org/userspace-api/landlock.html>; `cap-std`
  <https://github.com/bytecodealliance/cap-std>; TOCTOU <https://lwn.net/Articles/899543/>
- Supply-chain / agentic: `event-stream` post-mortem
  <https://snyk.io/blog/a-post-mortem-of-the-malicious-event-stream-backdoor/>; the Sept 2025
  npm chalk/debug compromise <https://www.wiz.io/blog/widespread-npm-supply-chain-attack-breaking-down-impact-scope-across-debug-chalk>;
  Shai-Hulud worm <https://unit42.paloaltonetworks.com/npm-supply-chain-attack/>; the lethal
  trifecta <https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/>
