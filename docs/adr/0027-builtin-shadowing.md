# ADR 0027 — When does `fn round(x)` start being `round`?

- **Status:** **Accepted 2026-08-13 — option (a): a shadow is file-scoped and retroactive.**
  Not yet implemented. A live three-engine divergence, reproduced and pinned below.
- **Date:** 2026-08-13
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0017 — Methods and functions](0017-methods-and-functions.md) (why the
  builtin namespace is flat and therefore shadowable at all),
  [ADR 0019 — Module system](0019-module-system.md) (define-before-use across files).

## Context

Helix lets a user function shadow a builtin. **The three engines disagree about when the
shadow takes effect**, and the disagreement is silent — a wrong number, exit 0.

The tree-walker resolves a name at CALL time. The VM and the JIT resolve at COMPILE time,
in source order. So a body compiled *above* a shadowing definition binds the builtin
forever, while the walker rebinds it as soon as the definition is reached:

```helix
fn use(v) = round(v)
print(use(1.4))     # VM/JIT: 1     walker: 1
fn round(x) = 99
print(use(1.4))     # VM/JIT: 1     walker: 99   <-- DIVERGES
```

```helix
fn round(n) = if n == 0 then 0 else abs(n - 1)
fn abs(n)   = if n == 0 then 1 else round(n - 1)
print(round(5))     # VM/JIT: 4     walker: 1    <-- DIVERGES
```

The second is worse than it looks: `round`'s body is compiled before `fn abs` exists, so
`abs` binds the *builtin* and the mutual recursion silently becomes a single step.

**This is pre-existing, and was proved so rather than assumed.** It reproduces identically
at `c16a9e7`, the parent of the two-pass registration work that surfaced it — that commit
rebuilt and produced byte-identical output for both programs. It is not a regression from
that change; it is what that change had to be careful *not* to make worse.

## Why it matters more than a corner case

The walker is the reference implementation. When the VM and JIT disagree with it, they are
wrong by definition — and this is the one place where "all three engines agree" (the
property the whole project is built on) is false for reasons nobody has decided to accept.

It has also already cost three separate guards, none of which fix it:

1. `mixed_fn_sigs` excludes builtin-shadowing names, because promoting one applied a user's
   `round` to the twenty call sites *above* its definition —
   `tests/corpus/j14_rounders_and_int_mixed.helix` caught `[99, 99, 99, 99]` against the
   walker's `[1, 2, 3, 4]` (`becf927`).
2. PASS ONE of two-pass function registration excludes them, for exactly the same reason
   (`db6941a`).
3. `body_raises` excludes a rounder shadowed by a user function, so the poison analysis does
   not mistake a user call for a raising builtin.

Three workarounds for one root cause is the signal that the root cause should be decided
instead of guarded. Each guard also *narrows* an optimization: every whole-AST, order-blind
analysis in the JIT has to carve out shadowing names, and each carve-out is a place a future
change can forget.

## The question

**Does a name mean one thing per file, or one thing per point in the file?**

## Options

**(a) A shadow is file-scoped and retroactive.** `fn round` means the user's `round`
everywhere in the file, including above its definition. The walker changes to match the
compiled engines (it is the one that would move), and every order-blind analysis in the JIT
becomes *correct* rather than guarded — all three carve-outs above can be deleted.
Consistent with two-pass registration, which already made `fn` definitions file-scoped for
ordinary names. Breaks any program that deliberately uses the builtin above its own
redefinition; `tests/corpus/j14_rounders_and_int_mixed.helix` does exactly that, on purpose.

**(b) Reject shadowing a builtin outright.** `fn round(x) = …` becomes an error suggesting a
different name. Kills the divergence and all three guards, and is the most "one obvious way"
answer. Strictly the most disruptive: it removes a capability that works today, and the
error has to be good enough that it does not feel arbitrary (the builtin namespace is large
and flat, so the collision is easy to hit by accident — which is also an argument *for*
rejecting).

**(c) Keep call-time semantics and make the VM/JIT match.** Emit a runtime-checked dispatch
for a shadowed name: check whether the global is bound yet, call the builtin if not. Every
current program keeps working. Costs a check per call to a shadowed builtin, keeps all three
guards, and keeps the order-blind analyses permanently unable to reason about these names.

## Decision: (a) — a shadow is file-scoped and retroactive

Accepted 2026-08-13, against the stated goal of *a language people build packages and
libraries on*. Two reasons, and the second is the one that decides it.

**It is the only option that makes the compiler correct rather than guarded.** Every
whole-AST, order-blind analysis in the JIT currently has to carve out shadowing names, and
each carve-out both narrows an optimization and leaves somewhere a future change can
forget. Under (a) all three guards are deleted, not added to. The one program that breaks,
`tests/corpus/j14_rounders_and_int_mixed.helix`, is a fixture written to pin this exact
behaviour — not a user program.

**(b) — rejecting the shadow outright — is disqualified by the ecosystem goal, and this
argument is new.** Under (b), every builtin name becomes reserved. Helix's builtin
namespace is flat (ADR-0017) and still growing, so *adding a builtin in a future release
would break any published library that happens to use that name as a function*. That turns
each new builtin into an ecosystem-wide breaking change and gives library authors a hazard
they cannot defend against — they would have to avoid names Helix has not chosen yet. Under
(a) the opposite holds: a user's `fn foo` keeps winning inside its own file no matter what
Helix adds later. **(a) is the option that lets the standard library grow without breaking
the ecosystem**, which is not a consideration that existed while Helix had no ecosystem to
protect.

(c) is rejected for the reason it was listed: it preserves every current program at the
cost of making these names permanently unanalyzable, which is paying forever to avoid
rewriting one test fixture.

## Consequences

- Under (a) or (b), the three guards are deleted in the same change, and
  `j14_rounders_and_int_mixed` is rewritten (its pinned output changes under (a); the file
  becomes an error case under (b)).
- Under (c), this ADR should record that the order-blind analyses will never cover shadowed
  names, so the next person does not try again.
- Whichever is chosen, `tests/ordering_matrix.rs`-style pinning is warranted: the divergence
  is silent, so it needs a test that fails loudly if it comes back.

## Open questions

- Does the same rule apply to shadowing a *method* (`xs.count()`) as to a free function?
  Methods dispatch on the receiver's type at runtime, so they may not have the problem at
  all — worth confirming before writing a rule that says "names" generally.
- Under (a), what happens across a module boundary — can an imported module's `export fn
  round` shadow the builtin for the importer, or only within its own file? ADR-0019's
  namespacing suggests the latter, and that should be stated.
