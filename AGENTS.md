# Working on / with Helix

Helix is self-describing — ask the binary before guessing:

```
helix search <term>       # FIND a capability by what it does — searches names,
                          #   signatures, docs AND notes. START HERE when you do
                          #   not know the name; `--json` for tools
helix describe <Type>     # ONE type's whole method table as JSON — sig, doc, example,
                          #   effect. ~6% of the full dump: ask this, not the catalog
helix describe <name>     # one builtin/method entry as JSON (every owner of the name)
helix describe            # the WHOLE API as JSON — 120 KB, rarely what you want
helix doc <Type>          # the same table, printed for a human
helix doc <name>          # a method or builtin by name: owners + an example receiver
helix doc builtins        # every free function
helix check file.helix    # fast type-check; never rejects a runnable program
helix check --json f.helix # the same diagnostics as data: line, col, message, hint,
                          #   AND the rendered prose. `--lint` notes come through too
helix effects f.helix     # what each function REACHES, closed over the call graph:
                          #   authority (fs/net/exec) and whether it is reproducible
helix jit-explain f.helix # which numeric kernels compiled, by line (`--json` too)
helix test <dir>          # runs *_test.helix files AND every `## >>>` doc example
helix test --engines <dir> # …and re-runs each on all three engines, failing on any
                          #   disagreement. Nothing else can check this; use it in CI
helix eval "print(1 + 2)" # one-liner
helix run tool.helix --n 3 # a script's own args bind to its `fn main` (ADR 0037)
helix run --watch app.helix # hot reload: rerun on save if it checks, keeping an app's state
helix run tool.helix --help # generated from `fn main` + its `##` doc; does NOT run it
helix fmt file.helix      # token-stream formatter; provably cannot change a program
```

The right loop for generated code is **generate → `helix check` → run**. If a method
might not exist, `helix doc <name>` answers in one command — this project's costliest
mistake was months of building around a "missing" `scan` that `helix doc Array` printed
all along.

Ask `helix search <word>` when you do not know the name, and `helix describe <Type>`
before writing against an unfamiliar type. Those are the two questions you have *before*
you know any names, and both cost a few KB rather than the 120 KB catalog. This is not a
hypothetical: two field reports independently resorted to dumping that catalog and
grepping it, and the words they had ("repeated header", "group by") were never the names
they needed (`get_all`, `frequencies`) — which is why `search` reads the docs and notes,
not just names. And read the diagnostics — in a 14-case sweep of the mistakes agents
actually make, **eleven named the exact fix** (`to_json(x)` answers "``to_json`` is a
method: `x.to_json()`"; a C-style body answers ``fn f(x) = x + 1``). Reading the error
is usually faster than reading anything else, this file included.

## Building an app (`import std.ui`)

Ask the binary, not the examples: `helix doc ui` lists every component with its parameters,
`helix doc ui.text_input` gives one in full (with the markup to write), `helix describe events`
lists every event and prop, `helix describe markup` the markup rules. `helix search bar chart`
finds components by what they do.

```helix
import std.ui

fn update(m, e) = match e.msg {
  "add" => {...m, items: m.items.concat([m.draft]), draft: ""},
  "draft" => {...m, draft: e.value ?? ""},
  "remove" => {...m, items: m.items.filter((x) => x != e.value)},
  _ => m,
}

fn view(m) = <div class="mx-auto max-w-md space-y-4 p-6">
  <form submit="add" class="flex gap-2">
    <input input="draft" value={m.draft} aria-label="New item" class="flex-1 rounded-control bg-surface-2 px-3 py-2" />
    <ui.Button caption="Add" msg="add" variant="primary" />
  </form>
  {if m.items.count() == 0 then <p class="text-muted">Nothing yet.</p>}
  <ul>{m.items.map(<li key={it} class="flex justify-between">{it}<button click="remove" value={it} aria-label="Remove {it}">×</button></li>)}</ul>
</div>

SESSION = [{msg: "draft", value: "milk"}, {msg: "add", value: missing}]
spec = {title: "List", init: {items: [], draft: ""}, update: update, view: view}
fn main(replay: Bool = false, port: Int = 8080) = if replay then ui.replay(spec, SESSION).map((h) => print(h)) else ui.app({...spec, port: port})
```

- **The loop:** `helix check app.helix` → `helix run app.helix --replay` on all three engines
  (outputs identical) → `helix run --watch app.helix --open` to look at it. A replay prints one
  screen per event, the state AFTER it; an error names the event that caused it.
- **Markup:** a lowercase tag is an element; a capitalised one calls a function by its parameter
  names (`<TodoItem done />` calls `todo_item(done: true)`; children are `children`); a library's
  is `<ui.TextInput …/>`. Attributes are HTML's lower-case names (`class`, `for`, `tabindex`),
  `aria-*`, and Helix's props. `class:done={c}` adds a class when `c`; `{if c then <X />}` needs
  no `else`; `{xs.map(<li>{it}</li>)}` loops. Mistyped props (`clik`), React names (`onClick`,
  `className`, `htmlFor`) and events Helix does not have (`blur`, `onMouseEnter`) are errors that
  name the fix.
- **Events** carry a message, not a handler: `click="save" value={item.id}` sends `{msg: "save",
  value: item.id}` to `update`, with the value's own type (a number stays a number). `input` is
  every keystroke (React's `onChange`); `submit` sends the form's named fields; `dismiss` is a
  press outside or Escape; keys are `keydown="key" shortcuts="Enter Escape Ctrl+K"`. Timers:
  `subscriptions: (m) => [ui.every(1000, "tick")]`, and `ui.after(800, "flip-back")` once.
- **Styling** is Tailwind's class names (v4), generated as the view renders, plus the design's
  tokens: `bg-accent`, `text-muted`, `bg-card`, `ring-line`, `rounded-card`, `p-card`,
  `shadow-1`. Dark mode is `theme: (m) => if m.dark then "dark" else "light"` and the tokens
  follow it. An unknown class renders nothing, silently — `helix check --lint` names the ones
  that look like utilities but are not.
- **Footguns:** a comparison with a `missing` field is `missing`, not `false` — give it a
  default first, `(m.plan ?? "") == "pro"`. The runtime supplies the page's `<main>`; it steps
  aside if your view's root is one. A busy port moves to the next free one, and says so.

## The correctness model

One program, one answer, on three engines: the tree-walker (`HELIX_NOVM=1`), the
bytecode VM (`HELIX_NOJIT=1`), and the Cranelift JIT (default). They must agree
**byte-identically** — values *and* error text. Differential check:

```
helix run p.helix > a.out; HELIX_NOJIT=1 helix run p.helix > b.out; HELIX_NOVM=1 helix run p.helix > c.out
cmp a.out b.out && cmp a.out c.out
```

**Any divergence is a Helix bug. Report it; never code around it.**

## Syntax that trips up agents

- Function bodies use `=`: `fn double(x) = x * 2`. Multi-statement bodies use `do { }`.
- `if c then a else b` is the expression form — no ternary, no parenthesized `if (c)`.
- **`match` exists** and is what a long `else if` ladder on one value should be:
  `match n { 0 => "none", x if x > 10 => "big", _ => "small" }` — literal arms, an
  optional `name if cond` guard, `_` for the rest, commas between.
- **`"""raw strings"""` do not interpolate and have no escapes.** THE form for a regex
  (`{4}` in an ordinary string is interpolation and silently becomes the number 4 —
  `helix check` refuses that), a Windows path, or any text with braces or backslashes.
- Strings have no `+`: use interpolation `"{a}{b}"` or `parts.join("")`. Both are linear.
- `{ }` in a string is interpolation; a literal brace is `{{`.
- **`/` always gives a Float**: `7 / 2` is `3.5` and `10 / 2` is `5.0`, so a score printed as
  `"{right / total}"` shows `0.5`. `//` is integer division (`7 // 2` is `3`) and `%` the
  remainder; `round(x, 1)` or `"{x:.1f}"` formats a Float.
- `try` binds tighter than operators: `try (a + b)`, never `try a + b`.
- One binding per line; `do { }` separates statements by newline, never `;`.
- `fn` is item-level only — inside `do { }` bind a lambda: `f = (x) => …`.
- A function value in a record field is called parenthesized: `(rec.f)(x)`.
- Imports are `import lib.stats` (for `lib/stats.helix`), `import lib.stats as st`, or
  `import lib.stats.{mean, sd}` to bring names in unqualified. Not `use`, not
  `from … import …`. **Only `export fn` / `export` names can be imported**: mark what a
  test file or another module needs (`export fn total(xs) = …`). **`import std.ui`** is built in (bound as `ui`): the app library —
  `helix describe app` for the whole surface, `examples/apps/gallery.helix` for every component.
- **`mut` is top-level only, and this is a design question, not a spelling one.** A
  function body evolves state by *rebinding* — `do { n = 0` / `n = n + 1` / `n }`, each
  line shadowing the last — and state that crosses a sequence is *threaded*, with
  `reduce`. Reach for `mut` only for state that must outlive a call. Deciding this after
  writing an imperative loop means rewriting the program, which is why it is here rather
  than left to the (very explicit) error.
- Most things are methods on a receiver, not free functions: `x.to_json()`,
  `s.parse_json()`, `xs.join(", ")`. `helix doc <name>` answers which in one command,
  and calling a method as a function tells you so by name.
- **Three ways to find something, in the order to try them.** `helix search <words>` when
  you know the JOB but not the name ("repeated header", "count occurrences", "session") —
  it covers builtins, methods AND the language forms above, every word has to match, and
  each row says which field it matched. `helix doc <Type>` for everything one receiver can
  do. `helix describe <name>` for one full entry, syntax included (`helix describe match`).
  Not finding something here is worth one more query before concluding it is absent: this
  project's costliest recorded mistake was months of building around a `scan` that
  `helix doc Array` printed all along.

## Footguns — wrong answers, not errors

1. **`missing` never equals anything, itself included** (`missing == missing` is `missing`).
   Comparing against the literal, `x == missing`, is refused with the fix: `x.is_missing()`,
   `where(@v.is_missing())`, or `drop_missing` (on Array and DataFrame). A *field* that may be
   missing still compares as missing, so give it a default first: `(m.plan ?? "") == "pro"`.
2. **i64 arithmetic wraps silently** — `9223372036854775807 + 1` is min-i64, exit 0.
   Deliberate; see `docs/integer-semantics.md`.
3. **`sum()` and its `reduce` spelling diverge at the i64 edge**: `sum()` widens to
   float where the reduce wraps. Documented divergence, same doc.
4. **Float `==` is exact**: `[0.1, 0.2].sum() == 0.3` is `false`. Use `assert_close`.
5. **Falling off a JIT kernel is silent** — same answer, much slower. If a hot loop is
   slow, suspect the shape, not the math: `helix jit-explain prog.helix` lists every
   kernel site the compiler offered the JIT, with its line and whether native code was
   generated, PLUS the functions compiled whole (a tail-recursive numeric function is
   native but is not a kernel site). A comprehension whose line is absent was never
   offered at all. It reports
   what the JIT was asked and what it answered, **not yet why** a shape was refused.

A lookup miss is *distinguishable* on request: `d.expect(k)` raises where `d.get(k)`
and `d[k]` return `missing` (ADR 0001 keeps the propagating default).

## Packaging

`helix.toml` is a full manifest — `helix new <name>` writes the template:

```toml
[package]
name = "physics"
version = "0.1.0"            # must be MAJOR.MINOR.PATCH — it is an ordering claim
description = "Orbital mechanics and physical constants"
authors = ["You <you@example.com>"]
license = "MIT"
helix = ">=0.2.1"            # toolchain floor — ENFORCED with one clear error
[dependencies]
```

The `helix` floor is the field to always set: an older binary opening the project says
*"this project requires Helix >= X, and this binary is Y"* once, instead of failing
sixty confusing ways on syntax it doesn't know. Dependencies are hash-pinned in
`helix.lock` (`helix sync` / `helix verify`).

## For contributors

- Gate before any commit: `bash scripts/gate.sh < /dev/null` (the stdin redirect is
  mandatory). **Never run `cargo fmt`.**
- Every fix ships with a regression test confirmed to FAIL on the previous binary
  first, on all three engines.
- Design decisions live in `docs/adr/`; read the ADR before changing a semantics.
