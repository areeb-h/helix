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
table, fifteen builtins and a new `Value` type. Note that `release.sh` would NOT have caught
this: it only refuses a patch whose `Unreleased` carries `### Changed`, and all of this is
`### Added`. Worth tightening — see the queue.

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

### 2. String accumulation in a record field

10.1× per 4×, 243 ms at n=128,000. Same shape as the dict case and milder. Node rendering IS
string building, so it matters for the UI library specifically. No lint names it yet — it has
no `dict()`-like literal to key on, so detecting it needs its own rule.

### 3. Two DataFrame blockers a storage engine hits immediately

Both reported from the field while building a chunked, reclusterable store. Both are
structural rather than cosmetic: they force a caller-supplied closure where the language
should have a verb.

**No row-offset slice.** A DataFrame has `head(n)` and nothing else — no `tail`, no
`slice(start, len)`. So a sorted frame cannot be cut into chunks, which is the whole of
"recluster by a column". `recluster` currently takes a caller-supplied `slicer` closure to
work around it.

**A DataFrame cannot be built with runtime column names.** `dataframe()` takes a RECORD, and
record fields are static syntax; `to_dataframe()` refuses an array of dicts. So there is no
generic way to construct a frame whose schema is known only at run time — which every
storage engine needs, because a chunk's schema comes from the data, not from the source
text. `dataframe(dict)` accepting string keys is the obvious shape.

Both need the polars and native backends to agree, so `dfdiff` is the net.

### 4. Tighten two guards that let something through

- **`release.sh`** only refuses a patch when `Unreleased` has `### Changed`. A
  language-surface addition under `### Added` is also a minor by policy and slips past.
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
