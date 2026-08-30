# Where we are, and what is next

A running note so the thread is not lost between sessions. **Newest first.** Everything here
is measured or gated unless it says otherwise — a claim without a number in this file is a
claim nobody checked.

Last updated: 2026-08-30.

---

## Released

- **v0.7.0** — tag on `4363df6`, published 2026-08-29. Regex that cannot hang the program,
  String predicates in a query (ADR 0039), a server that can hang up (a remote DoS three
  requests could trigger), peer + HTTP version on a request, a body sent in pieces, the
  capability sandbox findable and no longer failing open on a typo.

## In flight for the next release

Everything below is on `main` and **not** in v0.7.0.

**The next release is a MINOR (0.8.0), not a patch.** `docs/RELEASING.md` makes a
language-surface addition a minor by definition, and this cycle adds a `[workspace]` manifest
table, seventeen builtins, a new `Value` type and three DataFrame verbs.

`release.sh` WOULD have refused a patch here, but only by luck: it checks for a `### Changed`
heading, and `html_escape` happening to alter `to_html` bytes supplied one. Without that
single entry every other addition sits under `### Added` and would have slipped through. The
guard is right about `### Changed` and blind to "language-surface addition", which is the
other half of the same policy — see the queue.

---

## What landed since v0.7.0

### The module story (ADR 0040)

`helix.toml` carried two meanings at once — "this is a distributable package" and "imports
anchor here" — and `project_context` stops at the NEAREST one walking up. A repo with a
manifest per package therefore made each package its own module root, so `import ui.parse`
inside `ui/` looked for `ui/ui/parse.helix`. **A root manifest does not fix it** (that advice
was wrong; the nested one still wins). `[workspace] members = [...]` makes the root anchor
while members stay packages.

Also: `--lint` now walks the import graph (it read only the file it was handed, so a library
was never linted by any command — an O(n²) accumulation lived in helix-ui's `nn/train.helix`
for a whole release cycle because of it), and a failed import names the anchoring directory
and what set it.

### Accumulation (ADR 0029 amendment)

An array accumulated in a **record field** is linear now. At n=160,000, startup subtracted:
**2591 ms → 36 ms**, class quadratic (27.9× per 4×) → linear (3.3×).

`ArrayData::Shared` is an append-only buffer with a frozen per-value length. Sharing is safe
by construction: the buffer only grows, each value freezes its own length, so a value reads
only a settled prefix and two values cannot observe each other.

**Still quadratic, measured:**

| shape | 4n/n | n=128,000 |
|---|--:|--:|
| **dict in a record field** | 16.6× | **71 seconds** |
| string interpolated in a record field | 10.1× | 243 ms |

`helix check --lint` was narrowed to the dict case rather than deleted.

### The durable-storage substrate (ADR 0041)

Nine verbs: `rename`, `fsync`, `sync_dir`, `create_new`, `file_size`, `read_at`, `write_at`,
`truncate`, `remove_dir`. A durable commit is `write` → `fsync` → `rename` → `sync_dir`.

### Kernel-held locks (ADR 0041 amendment)

`lock_file` / `try_lock_file`. A `create_new` lock file does not release when its holder
crashes; these do, because the kernel hangs them on an open descriptor. Measured both ways:
after `kill -9`, the kernel lock is free and the lock file still reports busy.

### Row windows and runtime schemas (ADR 0043)

`DataFrame.tail(n)` and `DataFrame.slice(offset, len)`, and `dataframe(dict)`.

Two field-reported blockers that were the same defect twice: the language had no verb, so
the caller had to supply a closure. `tail` is NOT sugar over `slice` — expressing it as one
needs the row count, which a lazy frame does not cheaply have. Both windows CLAMP rather
than refuse (a window off the end is how a final partial chunk reads), so the clamping rule
now reads the same in `read_at`, `read_bytes_at` and the frame window. A negative offset IS
refused, because that question is `tail`'s.

`dataframe(dict)` yields columns in SORTED name order where a record keeps the written
order — a Dict has no insertion order, so inventing one would be a claim nothing supports.

Pinned by two corpus programs, which get `dfdiff` (both backends), `vmparity` (three
engines) and a golden at once. Both backends and all three engines were verified to agree
BEFORE the golden was pinned — pinning first would have recorded one backend's answer as
the language's.

### `html_escape`

One implementation, shared by the builtin and `to_html`. Scans before allocating, so text
with nothing to escape is returned unchanged rather than rebuilt four times. **Escapes the
apostrophe**, which the private copy did not — inside `<a title='...'>` an unescaped `'`
closes the attribute, so an escaper handling four of five is one you cannot rely on.
`&#39;`, not `&apos;` (the named form is undefined in HTML4). This is a `### Changed`:
`to_html` output containing `'` differs from v0.7.0.

### `Bytes` (ADR 0042)

`Value::Bytes`, ordered lexicographically by byte. `read_bytes`, `read_bytes_at`, `from_hex`,
`from_base64`, `"…".to_bytes()`, and a method surface mirroring `String`. Makes a
page-oriented **binary** store possible.

---

## The queue, roughly by value

### 1. The dict cliff — the worst remaining performance bug in the language

**71 seconds at n=128,000**, 16.6× per 4× the input. `Dict::insert` clones the whole
`BTreeMap` per call and `Op::InsertIntoLocal` rescues only the bare-local spelling.

The array fix does not transfer unchanged: an append-only buffer works because an array
appends at the END and a value's prefix is therefore settled forever. A `BTreeMap` insert
lands anywhere, and can OVERWRITE — so "settled prefix" has no analogue.

Two candidates, thought through but not built:

1. **A pending-inserts log**, which is the closest analogue. `insert` on a shared map returns
   `{ base, adds: Rc<RefCell<Vec<(DictKey, Value)>>>, len }`; reads merge base + adds once
   and cache. In a fold nothing reads until the end, so the whole fold is O(n) — exactly why
   it works for arrays.

   **The trap is `count()`.** It is not `base.len() + adds.len()`: an add that overwrites an
   existing key adds nothing. Either probe `base` per insert (O(log n) each, O(n log n)
   total — acceptable) or make `count()` materialize. Getting this wrong is a WRONG ANSWER,
   not a slow one, so whichever is chosen needs the differential oracle pointed at it.

   Also unlike arrays, an *older* view is not merely a prefix — it is base plus a prefix of
   `adds` — so branching must copy at exactly the right point.

2. **A persistent map (HAMT/CHAMP)** with structural sharing. The general answer, no cliff
   anywhere, and a much larger change to a core type.

Do NOT rush this into a release. It is the one remaining item where the failure mode is a
silently wrong dictionary, and `dfdiff` + `vmparity` + the corpus need to be the net. The
ADR 0029 amendment records the measurement to beat; the lint narrows again when it lands.

### 2. ADR 0028's bug survives in `select` / `sort` / `group` — needs an ADR

ADR 0028 fixed "a bare name means the COLUMN, not your binding" for `where` / `filter` /
`with`, on the grounds that otherwise a library author's parameter names become reserved
words in data they have never seen. It did not cover the COLUMN-NAME positions, and the same
bug is there. Measured:

A bare name here is taken LITERALLY. It is not resolved and then compared — so
`df.sort(v)` "working" at top level is a coincidence: the identifier happened to match a
column. It is not a parameter-only problem either; a top-level binding hits it too.

**The loud form** — a binding with no same-named column:

| written | result |
|---|---|
| `df.sort(@v)` | works |
| `df.sort(v)` at top level | works, BY COINCIDENCE — the ident matched a column |
| `df.sort("v")` | error: expected a column name |
| `k2 = "v"` then `df.sort(k2)` | error: no column `k2` |
| `d.sort(k)` inside a `fn` | error: `sort` needs its columns named at the call site |
| `d.select(k)` / `d.group(k)` | error: no column `k` |

**The silent form, which is the dangerous one.** When a column of the binding's own name
exists, the column wins with NO error — measured with `w = "v"` over
`{v: [2,1,0], w: [10,20,30]}`:

| written | answers | should be |
|---|---|---|
| `D.sort(w)` | `[2,1,0]` — sorted by **w** | `[0,1,2]` |
| `D.select(w)` | `["w"]` | `["v"]` |
| `G.group(w).sum(@n)` | **3 groups** — by w | 2 groups |
| `D.with({out: w})` | binding won ✓ | ADR 0028 working, the contrast |

`group` DOES have the silent form; it just needs an aggregation that takes a column
(`sum(@n)`) to reach — an arity check fires first otherwise.

So `fn top(frame, key) = frame.select(key)` does not merely fail on a caller's schema: on any
frame that happens to have a column called `key` — a plausible name in real data — it
silently returns the wrong column. The loud form costs a confused hour; the silent one costs
correctness.

Pinned by `tests/corpus/df_column_name_shadowing.helix`, whose golden is EXPECTED to change
when this is fixed, so the change arrives as a reviewable diff rather than a silent shift.

There is no way to name a column at run time in these positions at all; the field report's
store had to build a permutation with `sort_by` and rebuild through `dataframe(dict)`.

Three things are tangled and should be decided together, not patched apart:

1. Does a binding win in a column-name position, as ADR 0028 decided it does in an
   expression position? Consistency says yes; it is **breaking**, exactly as 0028 was.
2. Should a String be accepted there — `df.sort("v")` — given `df.column("v")` already takes
   one? That is the least surprising half and is NOT breaking.
3. Why does a bare ident resolve differently at top level than inside a function? That looks
   like a plain inconsistency rather than a decision.

**Already fixed, and separately:** reaching a column verb with evaluated arguments used to
report *"a DataFrame has no method `sort` — did you mean `sort`?"* — a message that
contradicts itself. It now says the verb needs its columns named at the call site, and that a
name held in a variable is not supported. That is the diagnostic; the decision above is the
fix.

### 3. String accumulation in a record field

10.1× per 4×, 243 ms at n=128,000. Same shape as the dict case and milder. Node rendering IS
string building, so it matters for the UI library specifically. No lint names it yet — it has
no `dict()`-like literal to key on, so detecting it needs its own rule.

### 4. Tighten two guards that let something through

- **`release.sh`** only refuses a patch when `Unreleased` has `### Changed`. A
  language-surface addition under `### Added` is also a minor by policy and slips past. In
  v0.8.0 the guard did fire — but only because `html_escape` happened to change `to_html`
  bytes; without that one entry, a release adding a manifest table, seventeen builtins and a
  new `Value` type could have shipped as a patch.
- **The panic ratchet** skips lines starting with `*` to ignore doc-comment continuations,
  which also skips `*stack.last_mut().expect(...) = …`. `vm.rs` has two uncounted. Tightening
  it will shift budgets across several files, so it is its own change.

### 5. Storage: what ADR 0041 still does not promise

- No `fsync` on an open handle (`fsync(path)` reopens; it cannot flush mid-write).
- No `O_DIRECT` and no defence against a device that lies about flushing.
- `Bytes` is not a dict key, has no JSON form, and cannot be a DataFrame column.

### 6. Smaller, still open

- Forward-reference arity ignores default parameters.
- `helix check --lint app.helix` output can be noisy now that it walks imports — the
  doc-example lint fires for every exported function in a library.

---

## Things to know before touching this

- **Gate with `bash scripts/gate.sh < /dev/null`**, never `cargo test`. Debug builds have
  three pre-existing failures (tree-walker stack overflow at 50k frames) that pass in
  release. Verified by stashing and reproducing on an untouched tree.
- **Never edit `src/` while a cargo build is running** — the result is a snapshot you cannot
  attribute. It cost one wasted 20-minute build this session.
- **`gh run view --json` reports `conclusion` as `""`, not `null`**, while a job runs. jq's
  `//` only replaces null/false, so `.conclusion // .status` prints blank and reads as
  "queued".
- **A backtick works fine in a Helix string literal** (`"a`b"`). Only `"\``" — an *escaped*
  backtick — fails. A report that says otherwise is almost certainly a shell heredoc eating
  it as command substitution.
- Adding a `Value` variant produces **no** exhaustive-match errors; every site has a
  catch-all. Probe each path by hand — dict key, JSON, operators, printing, sorting, three
  engines — rather than trusting the compiler's silence.
