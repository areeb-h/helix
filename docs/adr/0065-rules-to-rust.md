# ADR 0065 — Rules written and proved in Helix, shipped as Rust

- **Status:** Accepted (the freezer is done)
- **Date:** 2026-11-11
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0064](0064-helix-on-mobile-os.md), [ADR 0063](0063-the-app-platform.md),
  mobile-os `docs/helix.md` (item 1) and its ADR 0011 (the freezer).

## Context

mobile-os's privileged code is Rust and has to stay small and reviewable. A Helix runtime has no
place under the lock, the updater or the compositor. But those programs are full of RULES:
- which app to freeze, and when;
- when the phone may install an update and reboot;
- how long a PIN lockout lasts;
- which gesture outcome a release means.

A rule is a pure decision function, and that is where Helix is stronger than Rust. Rust can't
say that a function touches nothing; Helix proves it (`helix effects`). Rust tests check the
cases someone wrote; Helix can replay any decision on three engines and, as below, check every
reachable state.

mobile-os asked for "Helix is where the phone's rules are written and proved; what ships is
plain Rust generated from that", and gave the freezer's exact Rust API as the first contract.

## Decisions

### D1 — `helix emit rust`: a rule as plain Rust

`helix emit rust rule.helix -o policy.rs` writes:
- **Plain Rust:** edition 2024, `std` only, no `unsafe`, no dependencies, formatted as
  `cargo fmt` formats it, and clean under `cargo clippy -D warnings`.
- **Readable:** one Rust function per Helix function, the Helix names kept. The header gives
  the Helix version, the source path, its SHA-256 and the `helix effects` result.
- **The contract's own API:** the public methods are exactly the signatures the rule declares.

**The contract is written in the source**, in `## rust:` lines in Rust's own syntax. Helix
records are structural, and Rust needs a type for every value:

```text
## rust: pub enum Action { Foreground(String), Background(String), Freeze(String), Thaw(String) }
## rust: struct App { hidden_since: Option<i64>, frozen: bool }
## rust: pub struct Policy { after: i64, apps: BTreeMap<String, App> }
## rust: impl Policy
## rust: pub fn update(&mut self, now_ms: u64, open: &[String], visible: Option<&str>) -> Vec<Action>
export fn update(self, now_ms, open, visible) = …
```

How the Rust signature maps onto the Helix function:
- **`&mut self`:** the Helix function takes the state first and returns `(new_state, output)`,
  or only the new state when the method returns nothing.
- **`&self`:** it returns the output.
- **`-> Self` with no receiver:** it builds the state.
- **Integers:** inside they are `i64`, as in Helix. The public methods convert at the boundary
  (`u64` milliseconds, `&[String]`, `Option<&str>`).

**What translates, keeping Helix's meaning:**
- a record of a declared struct is that struct, and a record update is struct-update syntax;
- `{Freeze: id}` is `Action::Freeze(id)`;
- a Dict is a `BTreeMap`, so iteration is in key order as in Helix (the shell's `HashMap` was
  not);
- `missing` is `None`, `??` is `unwrap_or`, and `if x.is_missing() then … else …` unwraps `x`
  in the branch where it's present;
- integer arithmetic wraps, as Helix's does;
- where Helix would raise, the Rust panics with the same message.

**Refused, with the line and the fix:** anything outside that subset, a function without a
signature, arithmetic or comparison on a value that may be missing, and any rule that reaches
files, the network or programs. An emitted file is never a guess.

### D2 — The Rust is checked against the Helix

The emitter generates call sequences from the declared signatures and the rule's domains:
- `## rust-domain: String = mail, maps` sets the values used;
- `## rust-traces: 48` sets how many sequences.

It runs each sequence through the Helix functions and writes it as a Rust `#[test]` with the
outputs Helix gave. Then:
- **They must agree.** The Rust passes its own tests only if it agrees with the Helix it came
  from.
- **The output is deterministic** (a fixed seed), so `helix emit rust … -o policy.rs --check`
  regenerates the file and fails if the checked-in one differs. That is the CI check mobile-os
  asked for.

### D3 — `std.model`: every reachable state, not a sample

`import std.model` adds `model.explore(init, inputs, step, check)`:
- **The search:** a breadth-first search from the start state over a finite set of inputs. It
  checks a property on EVERY reachable step, and returns either "all hold" or a SHORTEST
  sequence of inputs that breaks one.
- **Small world, same code:** the rule's code is unchanged; only the world is small (two apps,
  a few clock readings, including readings that go backwards).
- **The freezer:** `examples/rules/freezer_model_test.helix` states eight properties and checks
  them on all 80 reachable states and 7,040 steps, in about 0.1 s:
  - the state keeps exactly the open apps;
  - the app on screen is never frozen, and is never sent a Freeze;
  - an app is frozen only after its full wait;
  - an app's CPU share is lowered before it is frozen;
  - a frozen app is thawed before it is shown or closed;
  - a frozen app always has the time it was hidden;
  - nothing freezes before the deadline `next_deadline` promised;
  - `thaw` reports exactly whether the app was frozen, and leaves it unfrozen.
- **Mutations caught.** Three realistic bugs were planted in copies of the rule: forgetting to
  thaw a shown app, freezing 5 ms early, and dropping closed apps without a thaw. Each was
  caught, with a 1- to 3-step counterexample.

This is the technique mobile-os's plan wants for the lock screen (its item 4), available now
for every rule.

### D4 — State that survives a restart: `Save<T>`

A signature can save and restore a declared struct:
- `-> Save<Saved>` returns it as text;
- `saved: Save<Saved>` takes the text that was read back, as `Option<&str>`. The rule's
  function sees `missing` when it was absent, damaged or edited.

The format is generated Rust. It has a header and version, one field per line, and a 64-bit
FNV-1a checksum of everything above it:

```text
helix-save 1 Saved
failures 3
wait_left 60000
check 8e1f…
```

`load` takes exactly what `save` writes and nothing else, and what `None` means is the rule's
decision. The emitted tests check this: a round trip, and `None` for every cut of the text and
for every single byte changed to `0`, `9`, `x`, a space, a newline or `-`.

### The PIN lockout (`examples/rules/lockout.helix`)

**What the rule does:**
- A guess is checked only when no wait is left. During a wait it is refused: not counted, and
  never accepted, right or not.
- The waits are 0 for the first four wrong PINs, then 1, 5, 15, 15 and 60 minutes. At ten, the
  phone must be wiped.
- Time is a monotonic clock the service passes in. `helix effects` proves the rule reads no
  clock itself, so setting the wall clock forward can't end a wait.
- A restart keeps the count and restarts the wait that was left. A missing or damaged save
  gives the longest wait and one guess before the wipe, never a fresh start.

**Properties**, checked on all 951 reachable states (11,412 steps):
- guesses right and wrong at several times, including a clock that runs backwards;
- restarts that keep, lose or damage the save.

**Mutations caught:** accepting a right PIN during a wait (two steps, found through a clock
that ran backwards), a damaged save giving a fresh start (one step), and a restart dropping
the wait (two steps).

**The emitted `Lockout`:**
- `new`, `restore(saved: Option<&str>, now_ms)`, `save(now_ms) -> String`, `wait_left`,
  `may_check`, `wipe_due` and `guess(now_ms, correct) -> Guess`;
- `Guess` is `Accepted | Rejected(wait) | Refused(wait)`;
- the constants, `Default`, and the Helix doc comments as `///` docs.

It is clippy- and fmt-clean, and its 48 replays and save-format test pass.

### The update and restart policy (`examples/rules/updates.helix`)

**What the rule does:**
- **Download** on an unmetered network with enough charge.
- **Install** into the other slot, waiting out calls and low battery.
- **Restart** only between 01:00 and 05:00, charging, at 50% battery or more, with the screen
  off, no call, nothing playing, and 30 minutes idle.
- **Ask** after three days of waiting, only with the screen on and no call. It never restarts
  on its own outside the window.
- **Inputs** are input records (`&Phone`, `&Update`) with public fields, and the hour is passed
  in.

**Properties**, checked on every reachable step: 24,576 steps, covering every combination of
conditions at the edge of each threshold, for an update at every stage. They include the
liveness property "when every condition allows a restart, it restarts".

**Mutations caught**, each in one step: restarting with the screen on, a night window one hour
too long, and asking before three days.

**Emitter support** this rule needed:
- `pub` fields on a `## rust:` struct;
- `&Struct` parameters;
- per-field test domains (`## rust-domain: Phone.hour = 0, 1, 2, 4, 5, 12, 23`);
- associated functions (`Policy::may_restart`).

## Results on the freezer

- **The emitted `Policy`** matches the API mobile-os specified (`new`, `update`,
  `next_deadline`, `not_frozen`, `thaw`).
- **Tests:** the shell's five policy tests, ported to milliseconds, pass on it, as do the 48
  generated replays.
- **Checks:** clippy is clean with `-D warnings`, `cargo fmt --check` is clean, and it compiles
  with `rustc --test -D warnings` alone. A Helix CLI test runs exactly that.
- **One behaviour change, on purpose:** several apps closing at once now thaw in name order
  every time; the shell's `HashMap` gave no order.

## How mobile-os uses it

```bash
cp examples/rules/freezer.helix  mobile-os/policies/freezer.helix
helix test --engines policies/                     # the rule's tests and properties, 3 engines
helix emit rust policies/freezer.helix -o shell/src/freezer_policy.rs
helix emit rust policies/freezer.helix -o shell/src/freezer_policy.rs --check   # in CI
```

The shell's `freezer.rs` keeps its systemd code and uses `freezer_policy::Policy`, converting
its `Instant`s to milliseconds.

## Consequences

- **The pipeline.** Rules are written in Helix, proved there, and emitted. The Rust is reviewed
  like any other generated code, and the proof travels with it: the properties live beside
  the rule.
- **A deliberately small subset.** It grows when the next rule needs something: the update and
  reboot policy, the PIN lockout schedule, gesture outcomes. Each addition comes with a test
  that the translation keeps Helix's meaning.
- **Clones over borrows.** The Rust is value-semantic, so it clones. For rules over a few apps
  that costs nothing that matters; a rule over large data would want borrows instead.
