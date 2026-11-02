# Where we are, and what is next

A running note so the thread is not lost between sessions. **Newest first.** Everything here
is measured or gated unless it says otherwise — a claim without a number in this file is a
claim nobody checked.

Last updated: 2026-11-02.

---

## Released

- **v0.10.0** — tag `v0.10.0`, published 2026-09-14. A program pays less for what it already
  knows: pure calls with literal arguments are evaluated when the program loads (ADR 0050), and
  a call site's record shapes, held names and array literals reach the callee as a clone reduced
  to the work the runtime values need (ADR 0051) — on the field build's ORM harness,
  `order+limit+offset` 2.698 → 0.702 µs from element knowledge alone and `where eq` 2.039 →
  0.830 µs from inlining record results alone, each an interleaved A/B of fresh builds with the
  statements byte-identical. A condition is a value
  (ADR 0052), so a library renders `@age > lo` and a frame takes it back by name. Two semantics
  changes by the user's decision: `/` is IEEE (ADR 0048) and spreads are the sample estimate by
  default (ADR 0049). Reductions along an axis carry gradients, so a row-wise softmax
  differentiates. Found by the field builds and fixed at the root in the same window: a clone
  typed through a freed address (§1.51), later call sites starved of their clones (§1.50), a
  binding called by name losing its binding (§1.53), a clone per recursion level (§1.60, a
  tokenizer 579 ms → 7 ms to load), and a duplicate top-level `fn` accepted in silence (§1.59).
  Release notes run as a program before the tag: 36 value claims and 10 refusals.
- **v0.9.0** — tag `v0.9.0`, published 2026-09-01. PostgreSQL spoken directly with
  TLS the server cannot turn off (ADR 0044); a method call resolved by its RECEIVER
  (ADR 0045), which is what makes a fluent library writable at all; polars retired to
  the oracle seat (ADR 0033 Stage 4); and the class of bug all three kept turning up —
  a decision made where the answer was not yet available. Measured against the previous
  tag, same profile: no wall or RSS regression on any crosslang workload, `b3_groupby`
  0.11s → **0.03s**, RSS down 4–39 MB.
- **v0.8.0** — tag on `289eb3d`, published 2026-08-30. The `[workspace]` manifest table
  (ADR 0040), the durable-storage substrate (ADR 0041) with kernel-held locks, `Bytes`
  (ADR 0042), row windows and runtime schemas (ADR 0043), `html_escape`, and the
  accumulation amendment to ADR 0029.
- **v0.7.0** — tag on `4363df6`, published 2026-08-29. Regex that cannot hang the program,
  String predicates in a query (ADR 0039), a server that can hang up (a remote DoS three
  requests could trigger), peer + HTTP version on a request, a body sent in pieces, the
  capability sandbox findable and no longer failing open on a typo.

**The release guard now asks the source, not just the prose.** `release.sh` used to refuse a
patch only when `Unreleased` had a `### Changed` heading, which a language-surface addition
under `### Added` slipped past (in v0.8.0 it fired only because `html_escape` happened to
change `to_html`'s bytes). It now also compares the builtin and method names in
`src/registry.rs` against the last tag and refuses a patch when the set grew. Still blind to
one thing: a SYNTAX addition — `[a, b] = xs` this cycle — adds no name, so the release that
carries one has to be cut as `minor` by hand. This cycle is one.

## In flight for the next release

**This cycle is a MINOR, not a patch** — positional destructuring is new syntax, and
`begin`, `commit`, `rollback` and `cursor` are new methods (so the
release guard would refuse a patch anyway). Gate green at **1,035 tests**
(608 lib + 392 CLI + 3 other + 32 dual-engine), **155** dfdiff programs with 0 undeclared
divergence, 99 files type-checked. The full list is `CHANGELOG.md`'s `Unreleased`; the shape
of it:

- **PostgreSQL grew up** (ADR 0044/0047 addenda). A transaction is a value that rolls back
  when dropped uncommitted (`tx = c.begin()`), and its `BEGIN` rides with its first statement
  instead of costing a round trip. A result larger than memory is read a page at a time
  (`c.cursor(sql, params?, batch?)`, `cur.next()`). Several statements share one round trip
  (`c.query([q1, q2, q3])`); an Array or Bytes is a parameter (`where id = any($1)`); a
  statement's time limit is the URL's `timeout=`; and a login is bound to the TLS session it
  runs over (SCRAM-SHA-256-PLUS, on by default). Statements are prepared once per connection
  and decoded where they arrive.
- **Positional destructuring** (ADR 0053) — `[a, b] = xs` in `let`, `do`, `where` and as a
  statement, with `...rest`; the old bare `a, b = xs` is now the same form.
- **A dict is linear to build AND to drain, and bounded by its keys** — `remove` appends a
  tombstone to the log `insert` writes, and the log settles once half of it is dead weight
  (queue item 1, below, is closed by it).
- **Specialization reaches further** — a recursion spends no depth, a call through a record
  built at run time is devirtualized, and a clone is remade for a shallower site only when the
  ceiling had cut it short.
- **Fixes**: a Bool column holds `missing` (and a nullable PostgreSQL `boolean` is a Bool
  column); `**` on floats follows one rule on every engine; `postgres_open` tries every
  address a name resolves to; `helix fmt` no longer writes `{...a,...b}`.

The four language-surface additions this section listed before (`df.rename`, a join type from
a binding, `has_feature`, `Tuple.count()`) shipped in **v0.10.0**.

The sections below shipped in **v0.9.0** and are kept for the reasoning, which is the part
that outlives the release.

### PostgreSQL, and TLS the server cannot turn off (ADR 0044)

`postgres_query(url, sql, params?)`, `postgres_open(url, mode?)` and — since 2026-09-04,
ADR 0047 — `postgres_execute(url, sql, params?)` behind `--features postgres`,
hand-rolled against the v3 wire protocol, **zero new crates**. Verified against a live
PostgreSQL 19 Beta 3: typed columns, `NULL` -> `missing`, parameters as values, read-only
enforced by the server (SQLSTATE 25006 on a write), SCRAM-SHA-256 with the server's own
signature checked, and `net` refused without the capability.

**The security decision worth arguing.** `libpq` and Go's `pgx` default to
`sslmode=prefer`: the client asks for TLS, and if the server answers `N` the session
continues in plaintext. Helix has two modes, not six -- `verify-full` (default) and
`disable` -- and no mode in which the SERVER can cause the downgrade. `require` and
`verify-ca` are refused with the reason each would have cost. Proven by the adversarial
case, not asserted: a listener that answers `N` makes both verbs fail, as a unit test that
needs no database (`src/pg/tls.rs`).

**Measured** (min of 7, load 1.41): TLS costs **+1.4 ms per connection and nothing per
query**. Five queries: 20.8 ms over five plaintext connections, 28.0 over five TLS ones,
**6.2 over one TLS connection**. The handshake is the query time, which is what makes the
connection value worth more than the transaction round trips it replaced (removing those
moved the number by 0.01 ms).

**A credential leak, found by writing the test.** `parse_url` echoed the whole raw URL --
password included -- into the error for an unparseable URL, which is the most widely copied
text a program produces. Fixed, and `the_password_never_appears_in_an_error` goes red
against the old line.

The URL policy lives in `src/pg/conninfo.rs`, deliberately OUTSIDE the feature gate, so a
build without the feature (the appliance) still runs its tests. Since 2026-09-04 the gate
builds `--features postgres` as well, so the wire-protocol tests — a fake server proving what
the client sends — run in every gate; until then they were the one feature whose tests ran
nowhere. `sslmode`'s accepted values are the security policy, and they are now pinned by
tests that run in every build.

**Writes (ADR 0047, 2026-09-04).** `postgres_execute(url, sql, params?)` and
`postgres_open(url, "write").execute(sql, params?)` answer `{affected, rows}`. A writable
session omits the read-only startup default and spends `db-write` (`HELIX_ALLOW_DB=write`)
as well as `net`, so granting the network alone still keeps a program read-only. Verified
here against a fake wire server; live verification is the field build's.

**The driver's own speed (2026-09-19/20, ADR 0044 addenda).** A connection prepares each
statement once, sends an exchange as one write, reads through a buffer that messages are
LENT from, parses cells from those bytes straight into the engine's vectors, remembers what
each statement returns (no second Describe; `int2/4/8`, `bool`, `float8` in binary from the
second run), and closes itself if an exchange does not reach `ReadyForQuery`. Live harness
against PostgreSQL 17 in a container: `target/bench/f89/`.

**Transactions (ADR 0047 addendum, 2026-09-20).** `tx = c.begin(isolation?)` is a connection
value that speaks for a transaction; `tx.commit()` / `tx.rollback()` end it, and one DROPPED
without committing rolls back — so an error between `begin` and `commit` undoes everything.
While it is open it is the only way into the session; a failed one cannot commit.

**Parameters and patience (2026-09-20).** An Array is a parameter (`= any($1)` with
`[[1, 2, 3]]`; Strings always quoted in the literal), and so is Bytes (`bytea`). The URL says
how long a statement may take: nothing (30 s of silence, as ever), `timeout=N` (the SERVER
ends it, `57014`, the connection carries on) or `timeout=0`; `connect_timeout=N`; TCP
keepalive on every connection.

**Several statements, one round trip (2026-09-21).** `c.query([q1, q2, q3])` / `c.execute([...])`:
framed together, ONE Sync — so all or nothing — an Array of answers; 5 lookups 998 -> 281 us.
Sent on a non-blocking socket while the answer is set aside, so it cannot deadlock (7 MB out /
14 MB back verified, clear and TLS). A `COPY` goes alone. The gate has TLS data-path tests
now: a certificate made at test time, rustls's server as the peer.

**The login is bound to the TLS session (2026-09-22).** `SCRAM-SHA-256-PLUS`
(`tls-server-end-point`), on by default and not something the network can switch off;
`channel_binding=require|disable` in the URL. A Bool column holds `missing`, so a nullable
`boolean` is a Bool column. `connect` tries every address a name resolves to.

**Positional destructuring (ADR 0053, 2026-09-23).** `[a, b] = xs` in `let`, `do`, `where` and
as a statement, `...rest`, the wrong length an error; the bare `a, b = xs` is the same form.

**A result larger than memory, a page at a time (2026-09-26).** `cur = c.cursor(sql, params?,
batch?)`; `cur.next()` a frame of at most `batch` rows, an empty one at the end (not
`missing`). A named portal Executed with a row limit — the plan and the formats `query` would
have had — inside a transaction of its own (begun with the first page, committed after the last,
rolled back by an early drop; the connection waits and says so) or inside the transaction it
was opened on, beside that transaction's statements. `type_of` says `Cursor`; `timeout=` bounds
a page. Live: ten pages of a million rows cost 0.79x the whole read. ADR 0044's last
functional honest cost. Found on the way and FIXED: transaction control went through the
prepared-statement cache, so a `commit` whose name went stale (`DEALLOCATE ALL`, a pooler's
other backend) failed its transaction and stranded the connection; it is never cached now.

**A transaction's BEGIN rides with its first statement (2026-09-26).** `begin()` sends
nothing; the first exchange carries the BEGIN (the snapshot is taken at the first statement
anyway). begin/update/commit 0.71x; an empty transaction never reaches the server; a stale
name in the first exchange is prepared again instead of failing the transaction (ADR 0047).

**Still open:** loading rows through `COPY … FROM STDIN` (a frame as the rows) — the driver
refuses every COPY, having nothing to hand the server.

### A method call is resolved by its receiver (ADR 0045)

**A fluent library was unwritable in Helix, and nothing said so.** UFCS was gated at parse
time on `registry::is_any_method` — a global test on a *name*, made where the receiver
does not exist. Every verb a query builder is made of is some type's method, so

    where  select  first  count  all  any  join  sort  take  drop
    insert  get  keys  values  filter  map  sum  min  max  unique

were invisible as user functions: `fn where(q, c)` two lines above `q.where(c)` failed
with ``type Record has no method `where` ``.

The decision moved to run time. A failed dispatch retries as `name(recv, args…)` against a
declared `fn`, then a builtin; a type that OWNS the name never falls back. The two
families the compiler routes by TYPE — `select`/`group`/`with`, and the comprehension
loops — have no dispatch to fail, so they emit BOTH readings behind a new
`Op::ReceiverIs`, with the receiver compiled once into a hidden local. The tree-walker
CALLS that opcode's predicate rather than restating it.

Gate green at **478 / 317 / 3 / 32**, dfdiff **136 programs, 0 divergences**, vmparity 0,
checkall 89/89.

**Two sabotages, both red.** Compiling the receiver into both branches turns the corpus
program into an outright VM-vs-tree-walker divergence; dropping `Missing` from `Iterable`
drifts it from its golden and fails the unit test. The FIRST version of the once-only
check did not go red — it watched only the branch that never received the duplicate. It
watches both sides now. That is the seventh time in this repo a guard has been found
unable to fail, and the remedy is the same one every time: break it and look.

**Found on the way.** `Op::DfColumnVerb` raised "expected a DataFrame, got Record" where
the walker raised "a Record has no method `select`" — a pre-existing divergence, reachable
whenever the checker cannot pin the receiver down. It calls the walker's own constructors
now.

**Still open, precisely.** The parser's desugars run before any receiver exists and still
win: `sort_by`, `min_by`, `max_by`, `argmin`, `argmax`, `take_while`, `drop_while`,
`zipmap`, `flat_map`, `count_where`, `position`. And `Op::GroupByAgg` still has the
divergence `Op::DfColumnVerb` just lost. Both recorded in `docs/dx-plan.md`.

### Polars is retired from the product (ADR 0033 Stage 4)

The default flipped: `default` and `bio` pull `native-df`, and polars stays behind the
`dataframes` feature **as the oracle only**. Binary 120 → **31 MB** (stripped 77 → **20**),
crates compiled 1,566 → **192**, startup 4.9 → **2.96 ms** like-for-like (**2.5 ms** for
the appliance build — not the same binary, so not the same row; a field build measuring
~4 ms was right not to reproduce the 2.5 this once claimed). Gate was green at
477 / 314 / 3 / 32 with dfdiff at **134 programs, 0 divergences** when this landed.

Keeping the oracle is the point, not a hedge: an engine cannot be its own evidence, so the
thing that says the replacement *means the same* has to outlive the thing it replaced.

**Every verb is faster.** 1.6M rows, materialised frames, every output consumed, min-of-7
(polars → native): `group` 20.7 → **5.2 ms** (4.00×), `join` 84.6 → **29.9** (2.82×),
`unique("col")` 9.0 → **3.2** (2.81×), `with` 49.0 → **19.8** (2.47×), `sort` 74.5 → **38.1**
(1.95×), `where` 26.0 → **13.6** (1.92×), `unique` 33.2 → **23.4** (1.42×).

One idea did most of it. **A hash table exists to map an arbitrary key onto a dense slot,
and a dictionary code already IS one** — `Col::Str` codes are dense in `[0, dict.len())` by
construction, so the distinct set is already computed and hashing the strings recomputes
what the column knows. An `I64` range is one scan away. The join's `Str` branch had done
this since Stage 3; it had simply never been generalized. `dense_domain`/`dense_slot` are
now one definition shared by `unique`, whole-row `unique` and `group`, because three copies
of that arithmetic are three chances to disagree about key identity.

**The flip exposed three undeclared divergences, and none of them was visible.** Native
refused ragged CSVs where polars padded them; native's join bypassed `validate_join_keys`,
the seam's shared diagnostic, which had exactly one caller; and an `allow(dead_code)` gated
on the wrong feature had been hiding the second. All three survived because **of 157 corpus
files, none read a CSV** and none joined on a bad key. A differential is evidence only for
what its corpus exercises. Five programs now cover it, taking dfdiff 129 → 134.

**The measurement lesson cost four wrong answers, one of them published.** A lazy engine's
fast number is usually a refusal: `read_csv(p).count()` parses nothing, `join(dim,@k).count()`
on a one-to-one join joins nothing, `sort(@x).count()` sorts nothing. Timed that way polars
read 0.11 ms for a sort costing 74 ms and 5.7 ms for a join costing 84 ms — turning two
native wins into reported losses. **The tell is sub-linear growth**: polars' "join" grew
1.12× for 4× the rows, which no join does. Every program in `bench/df/` now ends in
`.column(...)`.

**Still open.** `--features python` requires `pyo3-polars` — two call sites in
`src/python.rs`, the one configuration where polars reaches a shipped binary. The
replacement is the Arrow PyCapsule interface, which would also buy pandas/pyarrow/duckdb
interop; not taken because it needs either the arrow stack Stage 2 deliberately avoided or
a hand-written C Data Interface, to remove a dependency from a feature nobody gets unless
they ask for it.

### Terminal output that reads as carefully as it is computed

A field report arrived as two screenshots with the note *"looks a bit unaligned and weird"*,
which turned out to be three separate defects and one thing that is not a defect at all.

- **An axis tick is a POSITION, not a value.** `fmt_num` preserves what a reader needs to
  round-trip a result — correct for a printed value, wrong for a label. Reusing it made a
  sine plot's top tick read `9.974949866040545`: seventeen significant digits to say "about
  ten", in a gutter repeated on every row of the plot. `fmt_axis` gives three significant
  figures, keeps whole numbers whole (`10`, not `10.0`), and falls back to an exponent
  rather than growing without bound. **Gutter 21 columns → 8.** Histogram bucket bounds are
  deliberately untouched: `1.5–4` describes the data's own boundaries, which is a value.
- **A multi-line record starts every value at one column.** The multi-line form is only
  reached when a record is too wide to inline, which is exactly when a reader scans down the
  values rather than across one pair — and `count: 7` above `median: 3.0` above `min: 1.5`
  put them at three different columns. The pad is measured from the PLAIN key, since
  painting may have wrapped it in escapes that occupy no columns.
- **A bar carries its value at its own end.** Right-aligning the numbers bought a comparison
  the chart already makes — length IS the comparison in a bar chart — and paid for it by
  stranding each number up to a bar-width from its own bar. On `[1, 5, 2, 8]` at a wide
  terminal the `1` sat about a hundred columns away. Adjacent, the numbers trace the bars'
  own profile, so the comparison survives and the association returns.
- **`HELIX_PLOT=braille | blocks | ascii`,** for the reason `HELIX_BOX=ascii` exists.
  Measured first: every row a plot emits is *exactly* the same width in every glyph set
  (pinned by `plot_rows_are_column_exact`). So a sheared plot is the FONT rendering braille
  dots at a different width from the braille blank U+2800 they are padded with, and nothing
  on this side can repair that. What this side can do is offer a set the font certainly has.
  Resolution falls 2x4 → 2x2 → 1x4 per cell; the ASCII ramp keeps vertical position, so a
  rising curve still rises.

**What was NOT wrong**, and was nearly "fixed" three times: `HELIX_BOX=heavy` (invented —
the styles are rounded/square/ascii/none), the documented silent fallback on a mistyped box
style (a wrong border is self-announcing; a wrong CAPABILITY is not, which is why that one
fails hard instead), and `frequencies()` printing `[("a", 3), ("b", 2)]` — an array of
tuples is a value, its literal form is copy-pasteable, and it fits on one line.

Also on `main`: the capability denial names a grant that exists and carries the discovery
map, bare `helix` with no terminal refuses instead of hanging, and `helix build --runtime`
takes a bundle from 2.4 GB to 6.7 MB byte-identically.

---

## What landed in v0.8.0

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
| **dict in a record field** | 16.6× | **71 seconds** — linear since; see the queue, item 1 |
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

### 1. ~~The dict cliff~~ — fixed; what remains is a constant factor

**Done.** `Dict` is a settled base plus a shared append-only log (`src/value.rs`), the
"pending-inserts log" candidate below this item used to describe. `insert` appends; `remove`
appends a tombstone (it used to settle the view, 9.3 s at n=32,000 → 0.15 s); reads extend a
cache shared along the chain; and the log settles into a fresh base at power-of-two lengths
once half of it is dead weight, so an overwrite-only dict is bounded by its keys rather than
its writes (687 MB → 16 MB for ten keys over 4M writes). Pinned by `dict_log_semantics` and
`dict_log_tombstones` in the corpus and by three ratio/memory tests in `tests/cli.rs`
(`a_dict_read_between_inserts_is_linear`, `a_dict_drained_in_a_fold_is_linear`,
`an_overwritten_dict_does_not_grow_with_its_writes`). The `--lint` note on a dict in a record
field is retired.

Still open, deliberately: **branching off an older view copies the map** (once per branch, not
per write). A backtracking search that inserts into a parent's dict at every level pays O(n)
per sibling — what it always paid. A persistent map (HAMT/CHAMP) is the general answer and a
much larger change to a core type; nothing measured so far needs it.

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

### 4. ~~Tighten two guards that let something through~~ — done

Both closed: `release.sh` compares the registry's names against the last tag (with the
syntax-addition gap noted at the top of this file), and the panic ratchet in `tests/cli.rs`
tells a dereference `*x.unwrap()` from a comment continuation `* `, and counts `RefCell`
borrows too.

### 5. Storage: what ADR 0041 still does not promise

- No `fsync` on an open handle (`fsync(path)` reopens; it cannot flush mid-write).
- No `O_DIRECT` and no defence against a device that lies about flushing.
- `Bytes` is not a dict key, has no JSON form, and cannot be a DataFrame column.

### 6. Smaller, still open

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
