# Changelog

## Unreleased

### Added

- **`helix check --json`** — diagnostics as data: `{ok, checked, failed, files: [{file,
  ok, diagnostics: [{severity, file, line, col, message, hint, rendered}]}]}`. Tools
  were scraping caret-annotated text with regexes to recover a line number.

  It keeps `rendered` — byte-identical to the human output — rather than replacing prose
  with an error code. That is deliberate and measured: putting 14 mistakes an agent
  plausibly makes through `helix check` found **eleven whose help text names the exact
  fix** (`to_json(x)` answers *"`to_json` is a method: `x.to_json()`"*; a C-style body
  answers *"`fn f(x) = x + 1`"*), so the prose is the part that repairs the mistake and
  a machine format that dropped it would be a downgrade.

  The load path is what actually needed fixing: parse errors — the most common failure —
  were rendered to a `String` inside the module loader, so no caller could recover a line,
  a column or a hint. `module::Diag` now carries both halves, and `module::load` is a
  thin wrapper over it, so every existing caller is unchanged. `--lint` notes come
  through as `severity: "note"` and, as in the human output, change neither `ok` nor the
  exit code.
- **`helix describe <Type>`** — one receiver type's whole method table as JSON: per
  method the signature, doc, example, expected output, notes and capability effect, plus
  the universal methods. DataFrame's entry is ~6% of the size of the full catalog, so the
  question you have *before* you know any names is answerable without reading 120 KB.
  `helix doc <Type>` prints the same table for a human.

### Fixed

- **A Float now survives `to_json` → `parse_json` bit-identically.** It did not:
  `(-0.21453773034276893).to_json().parse_json()` answered `-0.2145377303427689` —
  one ULP away, and `!=` its own source literal. A model checkpoint written and read
  back was not the model that was saved.

  Neither half of the round trip was where the fault looked like it was.
  Serialization was always correct (`to_json` emits the shortest round-trip
  spelling), and Helix's own parser was always correct (`to_float` on that same
  17-digit string is exact). The loss was on the JSON *read* path: `serde_json` was
  declared without its `float_roundtrip` feature, so it parsed with a fast
  best-effort algorithm that is permitted to land a bit away. `0.1`, `0.2`, `0.3`,
  `pi` and `e` all round-tripped fine, which is why a hand-picked battery of "hard"
  values would have reported success — `src/json.rs` now proves the property over
  **20,000 random f64 bit patterns**, including subnormals and the extremes, and
  `tests/cli.rs` proves it survives the language surface on all three engines.

  Measured cost of the exact parser: **1.04×** on 300,000 floats (5 MB of JSON),
  26 ms → 27 ms, min-of-7.

## v0.6.0 — 2026-08-25

### Changed — one semantics: frames, arrays and scalars answer the same question (ADR 0036)

**This release changes answers.** ADR 0034 stated the doctrine — *a column expression
means exactly what the same expression means on scalars* — and then recorded three
deltas against the polars backend and deferred closing them. A release sweep found
**fifteen**, five of which were recorded nowhere — and running these notes against the
built artifact found a **sixteenth**. [ADR
0036](docs/adr/0036-one-semantics.md) closes all of them and replaces the delta list
with a rule: a divergence is a bug in whichever side disagrees with the language.

Every change below was verified on both DataFrame backends and all three engines.
`scripts/dfdiff.sh` runs every tracked program under both engines and reports **0
undeclared divergences**.

**Arithmetic in a frame now matches arithmetic on scalars.** On the polars backend
(the default):

| expression | was | now |
|---|---|---|
| `@a / 10` on `41` | `4` (integer division) | `4.1` |
| `@b / 10` on `[41, 38]` | `[4.1000000000000005, …]` | `[4.1, 3.8]` |
| `@a % -3` on `7` | `-2` (floored) | `1` (euclidean) |
| `@a // 2` | refused inside a query | `-4` (euclidean) |
| Int `@a / 0` | `missing` | error naming the row |
| Float `@a / 0.0` | `inf` | error naming the row |
| `0.0 / 0.0` | `NaN` | error naming the row |
| `@s + "y"` on a String column | `"xy"` | error, as on scalars |

The second row is the subtlest and affected every division by a constant in every
query: polars rewrites division-by-a-constant into multiplication by the reciprocal,
and `41.0 * 0.1` is not `41.0 / 10.0`. It only triggers at two rows or more, so a
one-row test reports agreement.

The **last** row is the one that got away and is worth knowing about. `+` `-` `*` `**`
lowered straight to polars' own operators — and polars' `+` on two `str` columns
concatenates — so `@s + "y"` answered `"xy"` with exit 0 while `"x" + "y"` was refused
on scalars and inside `map`. Every gate in the repo was green over it, for one reason:
no tracked program adds to a String column, so no differential run ever evaluated the
expression. It was caught by running this table against the release binary, which is
now part of the release ritual.

**Two of these change WHICH ROWS a query returns**, not how a number prints:
`where(@x / @y == 2)` on `x=[4,5], y=[2,2]` was 2 rows and is now 1; `where(@x / @y > 0)`
over a zero divisor was a silently shorter frame and is now an error.

**`.where(a).where(b)` is sequential** — the frame you filtered is the frame you filter
again. polars fused adjacent filters and evaluated both over every row, which was
invisible until a predicate could raise: `.where(not is_nan(@v)).where(@v > 2.0)` raised
on rows the first filter had already removed, so the guard the error tells you to write
did not work. Measured cost of the fix: 1.00x on a limited query, 1.04x on a streaming
write, 1.09x on a full scan.

**A NaN is a failed computation, not absent data**, and nothing converts one into the
other now:

| expression | was | now |
|---|---|---|
| `[1.0, nan, 3.0].max()` | `missing` | `NaN` |
| `[1.0, nan, 3.0].spread()` | **`2.0`** — a wrong number | `NaN` |
| `group(@g).max(@v)` over a NaN | `1.0` (skipped) | `NaN` |
| `[1.0, nan, 3.0].argmin()` | `missing` | `1` (the NaN's index) |
| `nan > 2.0`, `where(@v > 2.0)` | `true`, row kept | error + the guard to write |
| `[3.0, sqrt(-1.0), 1.0].sort()` | `[NaN, 1.0, 3.0]` | `[1.0, 3.0, NaN]` |
| `[nan, nan].unique()` | `[NaN, NaN]` | `[NaN]` |
| `1.0 % 0.0` | `NaN` | error |

`spread` was the worst: not missing, not NaN, but a plausible and confidently wrong
number in the stats surface, because it folded with Rust's `f64::min`/`f64::max` —
IEEE-754-2008 `minNum`, which ignores a NaN by design and was **removed** in 754-2019.

**Sorting places every NaN last, sign-independently.** The old rule ordered by sign bit,
which is unobservable from Helix and put the same printed value at both ends of one
sorted array: `[3.0, sqrt(-1.0), abs(sqrt(-1.0)), 1.0].sort()` was
`[NaN, 1.0, 3.0, NaN]`. `-0.0 < 0.0` is kept and now holds in a frame too.

**`==` stays IEEE** — `nan == nan` is `false` at every depth. Keys are a *separate*
relation in which all NaNs are one identity, which is what makes `unique` idempotent
and a hash join implementable. Both relations are now named and cannot leak into each
other. This knowingly reverses one clause of ADR 0001's 2026-07-17 amendment; the
argument is that the clause was never implemented — arrays obeyed it, frames did not.

### Added

- **`nan`** joins `pi`/`e`/`inf` as a literal. If your program binds `nan` as a
  variable name, rename it (`mut nan = …` still shadows, as for any constant).
- **`.drop_nan()`** on arrays and DataFrames — the single visible opt-out from NaN
  propagation, parallel to `.drop_missing()`. `xs.drop_nan().max()` is the `nanmax`
  spelling. The two verbs remove different things: neither touches the other's value.
- **`is_nan()` and `is_finite()` work inside a DataFrame query**, in both the
  free-function and method spellings. Until now they were a parse-time refusal on a
  column, which made the runtime's own advice — "guard it first with `is_nan(x)`" —
  impossible to follow where it was most needed.
- **`HELIX_DF_ENGINE` is validated.** Naming an engine the build does not have was
  silently ignored, so `HELIX_DF_ENGINE=native` on a released binary gave you polars
  with no diagnostic — and the two disagreed. It now refuses by name and says what the
  build contains.

### Testing — the gates that were not gating

- **The dual-engine DataFrame campaign now runs.** 28 tests in
  `src/backend/native/tests.rs`, including every `mod against_the_oracle` comparison
  against the polars oracle, executed in **no gate at all**: `native-df` is not a
  default feature, `scripts/gate.sh` ran a bare `cargo test`, and CI's only `native-df`
  step was a `clippy` without `--all-targets`, so the test targets were never compiled.
  They were written, reviewed, committed, and run by nothing — while `docs/testing.md`
  told readers they ran through the gate. The gate now runs them in their own target
  directory; CI runs the full `native-df` suite on a compile it was already paying for.
- **Version-compatibility baselines** (`tests/compat/`): what a released version
  actually computed — exit, stdout, stderr for 119 deterministic programs — captured by
  `scripts/capture-compat.sh` and **never rewritten**. Every other gate in this repo
  compares the tree against itself and so proves only consistency; this is the only one
  that can answer "does the program I wrote six months ago still compute the same
  number?". There is deliberately no environment variable that blesses a drift: an
  intentional change is recorded in `tests/compat/MIGRATIONS.md` with its reason, and
  that file accumulates into a checkable list of every user-visible behavior change.
  This matters now specifically — ADR 0033 Stage 4, ADR 0034's arithmetic deltas, the
  row-order-to-Neumaier switch, and the 0.6.0 polars tightenings are all queued to
  change printed numbers, and nothing recorded what v0.5 did.

## v0.5.1 — 2026-08-24

### Fixed

- **Int↔Float comparison is exact above 2^53** on every engine and every path
  (`==`, ordering, `min`/`max`, `sort`, `unique`, `frequencies`): the widening
  collapse (`i64 as f64`) made `[2^53+1, (2^53).0].max()` answer the strictly
  smaller number depending on order, and called two different numbers equal —
  equality is transitive again (`interp::ops::int_float_cmp`).
- **`helix check` catches guaranteed rebind errors**: `x = 1` then `x = 2`,
  `pi = 3`, and `mut f = ...` over a reached `fn` all checked "ok" and died at
  run time — the checker now refuses them with the runtime's exact wording,
  while every legal shadowing idiom (mut rebind chains, re-declaring `mut`,
  `mut` before a same-named `fn`, duplicate `fn`, `mut e = ...`) stays legal.
- **`helix check` catches method arity** for String/DNA/Array/Record receivers,
  fed by the same signature strings `helix doc` prints (`"ATG".upper(1, 2)`
  checked "ok" and died at run time).
- **Module files can no longer shadow the seeded constants**: `export pi = 3`
  (or `fn pi()`) in a module silently rebound pi module-wide, where the same
  line in a single-file program refuses; the loader now refuses identically
  (`mut pi = ...` remains an explicit, legal shadow).
- **`import m.{f}` keeps f's parameter defaults** — the selective spelling
  silently dropped what the qualified call kept, so `greet("ada")` was an arity
  error. Named arguments on a selective import now get an honest error naming
  the situation instead of calling `f` a builtin.
- **Native frame fixes**, pinned by dual-backend tests: CSV round-trips the
  empty string (`""` is a string, a bare empty field is missing — both
  directions were lossy); integer-looking CSV fields too big for i64 stay text
  instead of rounding through 1e20; `-0.0`/`0.0` collapse as one group/join/
  unique key; a join whose key dtypes differ refuses instead of answering an
  empty frame; `group(...).min/max` on a String column answers lexically.
- **`respond` validates `status`**: `status: 9999` wrote a protocol-invalid
  wire line and a non-Int status silently became an empty 200, discarding the
  payload — both now refuse with `status must be an integer between 100 and
  599`. A present-but-wrong `jar:` on `http_request` is a teaching error
  instead of a silent cookieless request. Duplicate `df.select(@a, @a)` refuses
  in Helix's own words on the polars backend too (its error recommended
  `.alias(...)`, an API Helix does not have).
- **`[].norm()` is `+0.0`** (Rust's empty float sum is `-0.0`, and
  `sqrt(-0.0)` is `-0.0` — a negative empty-vector norm).
- **Message and doc polish**: `parse_cookies` docs say what it returns (a Dict,
  last value wins); the `to_dataframe([])` hint no longer recommends a refused
  idiom; record field-list hints print in canonical sorted order; the static
  slice help names tensors; `helix test --json` emits a JSON document even for
  a missing path; multi-line doc-test failures keep their indent.

## v0.5.0 — 2026-08-23

### Performance — the native engine wins its own matrix (ADR 0033 Stages 2–3)

- **Native is faster than the polars backend on all 16 verbs** of the 5M-row
  matrix on the dev box (min-of-3, every result cell-compared against the polars
  backend). Ratios are native's time as a fraction of the polars backend's:
  filters ~0.01x, with-arithmetic 0.14x, groups 0.36–0.48x, join 0.85x, unique
  0.58x, sort + parquet write 0.53x. One machine, one workload, and the
  comparison is against our own polars backend's use of a lazy query engine —
  not a universal "faster than polars" claim.
- **The crossover happened at the 1M anchor first** (ADR 0033 Stage 3), then the
  full matrix fell in four passes: dictionary-encoded string columns, hand-built
  parquet pages, lazy per-column decode, and page-level predicate pushdown (the
  predicate runs once per distinct dictionary value, not once per row). polars
  remains the default backend and the oracle — Stage 4 (flipping the default) is
  deliberately not taken.

### Added — the native engine reads and writes parquet, and CSV goes parallel (ADR 0033 Stage 2)

- **A native parquet reader and writer** (zstd): the appliance build reads and
  writes the sibling engine's files. The writer hand-rolls the RLE codec and
  page-level IO (write_parquet 0.38x); the reader decodes columns lazily with
  deferred gathers — `count()` reads the footer only, a full-materialize read
  lands at 0.01x, a filtered scan at 0.58x.
- **CSV is parallel in both directions**: write_csv 0.39x, read_csv 0.84x
  against the polars backend on the same matrix.

### Added — the consolidated field review, answered (2026-08-24)

- **The differentiable surface closes over the elementary family.** Every routed
  unary (tan, asin, acos, atan, sinh, cosh, log2, log10, cbrt, degrees, radians,
  erf, normal_cdf, normal_pdf) joins relu/sigmoid/tanh/exp/ln/sqrt/sin/cos/abs on
  the tape; `max`/`min`/`clamp`/`hypot` carry gradients (ties route to the FIRST
  argument — the same convention relu's kink sets, so `max(a, b)` and the field
  idiom `a + relu(b - a)` agree everywhere); Array `.max()`/`.min()` fold tracked
  elements; **unary minus works on a tracked value** (`-v` is `0.0 - v`); and the
  method and free spellings agree — `v.to_array()` and `v.tan()` fall through to
  the free builtins, while tape-owned names keep the tape's errors. What still
  refuses (floor/ceil/trunc/round/sign) refuses HONESTLY: the error names the op
  and the way out (`value_of`), never "expected a number, found a Node".
- **An unrecognized field in an `http_request`/`http_stream` record is a hard
  error** naming the field and listing the ones read — `cookies:` (for `jar:`)
  had silently produced a cookie-less session; a typo'd `timeout_ms` left a
  request with no total deadline. The helix.toml unknown-key rule, one layer down.
- **`headers(pairs)`** constructs the case-insensitive Headers type (wire order,
  repeats kept, injection-validated) so test doubles can be the type live
  responses carry. **`url_decode_lenient`** never raises on any string (malformed
  `%` stays literal) — the server-edge twin of the strict decoder.
  **`url_encode(s, set)`** names RFC 3986's grammars ("segment", "query",
  "fragment", "userinfo"). **`to_dataframe(rows)`** builds a frame natively from
  an array of records (previously python-gated with no bridge at all).
  **`flat_map`/`count_where`** (parser desugars), String **`replace_first`** and
  **`last_index_of`** (character index, like `index_of`). `dna` is idempotent.
- **`helix describe <name>`** answers about ONE name — with a signature, a doc
  sentence, ONE executed example, a `differentiable` flag, and a `notes` channel
  for semantic surprises (last-wins, raises-on, seed-threaded); `helix doc <Type>`
  lists per-method signatures and doc lines. Every example with an output is run
  by the gate, so the documentation cannot drift. `helix test` states its version
  first, on every path.
- **Errors that teach, per the review's evidence**: a failed format spec names
  the `{{`/`}}` escape (the CSS/JSON brace trap); `try(f()).ok` explains that
  `try` binds tighter and how to bind first; a doc-example failure with a `...`
  continuation states the one-line rule.

### Added — tooling, organization, and the generated reference (2026-08-24)

- **`where` clauses** (ADR 0035): `fn f(c) = LOOKUP.get(c) ?? "…" where LOOKUP = …`
  — the scaffolding after the point, desugared to `let … in` at parse time (the
  engines cannot drift). fn definitions only; `where` remains an ordinary name.
- **`helix test --json`** — one JSON document (version, totals, per-event
  file/line/code/expected/got) with exit codes identical to the prose mode.
- **`helix check --lint`** — advisory notes for the field corpus's real traps
  (`reduce(dict(), …)` with the last-wins warning, `0 - x` now that unary
  minus is universal, `export fn` without an executable doc example). Never
  changes the exit code.
- **`helix doc --markdown` and docs/reference.md** — the full stdlib reference
  (357 names: signature, doc line, notes, executed example) generated from the
  docs table; a gate test regenerates and byte-compares it, so the committed
  reference cannot go stale.
- **A tracked exponent differentiates**: the full `a ** b` node (d/db =
  a^b·ln a) replaces the refusal; a non-positive base under a tracked exponent
  still refuses, saying why. The receiver lift is gated on the tape's own
  method names, and a tracked value in a plain op's error is called "a tracked
  value" with a pointer at describe's `differentiable` flag.
- **The codebase reorganized**: `interp/builtins` (11 topical modules) and
  `interp/methods` (per-type files) replace the two fattest files in the tree —
  verbatim moves, gate-proven behavior-identical. `scripts/release.sh` +
  docs/RELEASING.md codify the versioning policy and release ritual;
  `scripts/gate.sh` prints per-phase timings and gains a loudly-labeled
  `GATE_QUICK=1` iteration loop.

### Changed

- **Records print in canonical sorted field order.** `==` always ignored order
  and `to_json` always sorted; now the printer agrees, so a doc example
  documents the value rather than the construction route — `{name: "gc",
  arguments: 1}` and its `parse_json` twin both print
  `{arguments: 1, name: "gc"}`. A program that depended on insertion-order
  printing must sort its expectations once.
- **The frozen frame footer singularizes**: a one-row frame prints `(1 row)`.
  This is a versioned change to the frozen format (spec rule 5), made now,
  before the plural could ossify as a permanent cosmetic wart.

### Fixed

- **A tz-aware timestamp no longer aborts the process**: the polars backend
  prints tz-aware datetimes as UTC text instead of panicking mid-print.

## v0.4.0 — 2026-08-23

**The trust release.** Everything here is about what a program can rely on: an HTTP
client that is secure by default (ADR 0031, all four steps), frames whose semantics
are the language's own (ADR 0033/0034), printed output no environment variable can
bend, binaries that start on a 2022 Linux and fit on a small one, and a method call
that can never capture the wrong thing. The minor version moves because
`print(df)`'s bytes changed — everything else is additive or stricter.

### Security — the HTTP client earns its defaults (ADR 0031)

- **Header injection is refused before the wire**, both directions: a CR/LF (or any
  C0 control) in a header name or value is a positioned error at construction —
  request and response alike.
- **`Headers` is a type**, not a Dict: case-insensitive lookup (`get`/`get_all`/
  `has`/`keys`/`values`/`items`), wire order preserved, repeated headers kept — an
  HTTP/2-style lowercase response and an HTTP/1.1 one read identically.
- **Per-request `total_ms`/`connect_ms`/`read_ms`/`max_body`** — every limit that
  trips is an error naming itself, never a truncated body pretending completeness.
- **Redirects enforce the boundary rules**: `Authorization`/`Cookie`/
  `Proxy-Authorization` stripped when the origin changes (not configurable),
  `https` never silently downgrades, non-http(s) schemes refused, ten hops max,
  and the chain comes back as data (`redirects`). QUERY keeps its method and body
  through 301/302 per RFC 10008.
- **A cookie jar** (`cookie_jar()`), explicit program-held state threaded per
  request — never ambient. Supercookies are structurally refused via the Public
  Suffix List (`Domain=.co.uk` falls back to host-only); `Secure` never crosses
  plain http; `Max-Age` beats `Expires`; expired cookies evict on read and write.

### Added — the appliance profile (ADR 0032)

`--no-default-features --features appliance` builds a **9.3 MB** helix (was
51.8 MB) with the FULL language surface: every DataFrame, genomics, and JIT verb
still exists, type-checks, and describes itself — running one names the feature to
rebuild with. Feature gates: `dataframes` (polars), `bio` (noodles + needletail),
`jit` (cranelift; bytecode is identical either way — the flag changes speed, never
output), `native-df` (the new engine below, included in appliance). Defaults
unchanged: `cargo install helix` is still the full flagship.

### Changed — printed DataFrames have a Helix-owned format (ADR 0033, Stage 0)

`print(df)` and `"{df}"` interpolation now emit Helix's own frozen table text
instead of the engine's (polars') display:

```text
region  samples    af
------  -------  ----
east         12   0.5
(1 rows)
```

Why it changed: the old text was whatever the DataFrame engine printed — it varied
with `POLARS_FMT_*` environment variables (the same program, different bytes, based
on the caller's environment) and would have changed again with any engine change.
The new format is deterministic, environment-insensitive, engine-independent, and
identical across all three engines; cells format exactly as the language's own
scalars (`2.0` floats, `missing`, ungrouped integers). Scripts that parsed the old
box-drawn table must switch to `write_csv`/`to_json` (which were always the stable
interfaces) or the new format. Interactive rich rendering is unchanged.

### Added — a native DataFrame engine (ADR 0033 Stage 1, ADR 0034)

Builds without polars can now carry working DataFrames: `--features native-df`
brings an eager, deterministic engine whose column expressions run through the
interpreter's own scalar kernel — a frame expression means exactly what the same
expression means on scalars, including euclidean `%`, true division, and
division-by-zero as a positioned error naming the row. Aggregations implement
the missing-propagation doctrine directly; joins and groups are deterministic by
construction. The appliance profile now includes it: full frame pipelines
(read_csv/where/with/sort/group/join/write_csv) in a 9.3 MB binary. The default
build still uses polars; a dual build (`--features native-df` on top of default)
runs both engines and is differential-tested verb by verb.

### Fixed

- **UFCS could capture a Python attribute call**: `np.round(1.5)` was rewritten to
  `round(np, 1.5)` because dynamic-dispatch receivers resolve attributes at run
  time. The parser rewrite now covers user-defined functions only; builtin
  chaining is restored by a runtime fallback that fires only after method dispatch
  fails on the receiver — provably additive, and excluded for PyObject/Node/
  DataFrame/GroupBy receivers.
- **The event-loop server's request headers** had drifted to a case-sensitive Dict
  on the concurrent path while the blocking path got the `Headers` type —
  `get("content-type")` missed at 55k req/s. One shared parser now serves both.
- **Release binaries start on 2022-era Linux again**: the gnu artifacts silently
  inherited the CI runner's glibc 2.39 floor (locking out Ubuntu 22.04, Debian 12,
  and every RHEL); both gnu builds are now pinned to a 2.35 floor, the workflow
  asserts the artifact matches what the installer advertises, and `install.sh`
  auto-selects the static musl build on musl distros and any glibc below the floor.

### Performance

- **Serving**: `listen(port, shards)` composed with the event loop measures
  317–336k req/s on a 6-core box (~2x a node 24 cluster at equal workers), p50
  0.29 ms — near-linear scaling; the previously recorded ~90k ceiling was a host
  power-management measurement artifact, now documented.
- **Old hardware**: serving 100 connections fits under a hard 20 MB memory cap at
  full throughput; a 40%-throttled core sustains 21k req/s. Keep-alive buffers
  shrink after large bodies (was: 300 idle connections could pin 300 MB), inbound
  reads gained a 64 MiB per-shard budget mirroring the SSE side, and every eval
  thread's stack reservation dropped 1 GiB -> 128 MiB (measured ~1 KiB/frame;
  strict-overcommit VPSes can actually spawn shards now; `HELIX_STACK_MB`
  overrides).

## v0.3.0 — 2026-08-20

**The language-surface release.** A 13-library / 117-module / 15,260-line review of
v0.2.7 arrived with its claims already probed against the released binary, and this is
the answer to it — plus the tensor bridge the nn build was blocked on, and the pieces a
web library could not be written without. The minor version moves because programs can
now say things they could not say before, not because anything they said has changed.

### Added — the language

- **A function can be called in method position.** `x.f(a)` means `f(x, a)` when `f` is
  no type's method, so your own functions chain like built-in ones:
  `{w: 2, h: 3}.scaled(2).area()`. This removes the method-vs-function split rather than
  documenting it — the review's complaint was that `to_array(t)` is a function,
  `a.matmul(b)` is a method, and you learn which one error at a time. It is also what
  people usually want from classes: methods on your own types, with no new kind of
  entity, no inheritance, and no mutable object identity. Strictly additive by
  construction: the rewrite is gated on the name belonging to no type, so no call that
  resolved before can change meaning, and a misspelled method still gets the method
  error with its did-you-mean.
- **Range patterns in `match`** — `200..300 => "success"`. Half-open, `lo <= x < hi`,
  the convention `range(lo, hi)` and `xs[lo:hi]` already use, so adjacent bands tile
  exactly: nothing lands in two of them and nothing falls between. A range asks about
  magnitude, so `2.5` matches `0..5`; literal patterns keep their existing strictness.
  An impossible range (`5..0`, `3..3`) is refused where it is written.
- **Dict literals** — `{"Content-Type": "application/json"}`. A quoted key makes a brace
  a Dict; a bare name still makes it a Record. A record field is a NAME, so a map whose
  keys are not names — every HTTP header, every JSON object, every table transcribed
  from a document — could previously only be written
  `[("Content-Type", "…")].to_dict()`, a constructor shaped like a fold.
- **Block comments** — `#[ … ]#`, spanning lines, and **nesting**, so a region that
  already contains one can be commented out whole.
- **The scalars→tensor bridge.** `tensor([[w11, w12], [w21, w22]])` over tracked
  scalars builds a tracked tensor, so a trainable layer's weights can be ordinary
  variables and its forward pass an ordinary BLAS `matmul`. `t[i]` and `t[a:b]` on a
  tracked tensor stay on the tape, and `shape`/`count`/`ndim` read the value.

### Added — the library

- `s.split_once(sep)` → `(before, after)` or `missing`. Splitting at the FIRST separator
  is the commonest parsing step there is and had no direct spelling; the idiom it
  replaces recovered the tail by arithmetic on the first part's length.
- `s.index_of(needle)` → the first match's **character** index, the unit every other
  String method counts in, so it feeds straight back into `drop`/`take`/`s[a:b]`.
- `xs.windows(n)` and `xs.chunks(n)` — `Dna` had `windows`; an array had to hand-roll it.
- `s.concat(t)` — the sibling of `Array.concat`.
- `url_encode` / `url_decode` (RFC 3986). Without them a query string cannot be built
  correctly, and the hand-rolled version cannot be right: percent-encoding is defined
  over UTF-8 **bytes**, so `café` must become `caf%C3%A9`.
- `parse_cookies` and `parse_set_cookie`. Builtins rather than string code because the
  naive version is wrong in a way that looks right — an `Expires` attribute contains a
  comma, so splitting `Set-Cookie` on `,` tears the date in half.
- `to_dict` now takes a pair **however it is written**: `(k, v)` or a two-element array.
  That is why the review found seventeen `reduce(dict(), …)` folds — tables transcribed
  as arrays of arrays could not use the verb that exists.
- `{...dict, k: v}` — a Dict spreads into a record, the request-builder shape.

### Fixed

- **Three shapes that could produce a silently wrong program**, all now refused at check
  time: `try(() => f())` reported SUCCESS (building a closure cannot fail, so error
  handling written that way never fired); `fn relu(x) = relu(x)` defined infinite
  recursion under a shadowed builtin; and a top-level value used above its binding said
  only "not defined", with no hint that a `fn` may be used above its definition and a
  value may not.
- **HTTP connections are reused.** `get`/`post`/`request` each built a fresh agent, and
  the agent IS the connection pool — every call opened a new TCP connection and redid
  the TLS handshake. For a loop against one API host that handshake dominated the
  request.
- **The recursion-depth error names the cause**: the cap only ever binds a NON-tail
  shape, since tail and mutual-tail recursion run to millions of levels, and the message
  never said so.
- **`helix fmt`** was rendering `xs[0:2]` as `xs[0: 2]`, `match x { … }` as
  `match x {…}`, and `try (a / b)` as `try(a / b)` — three cases of a rule written for
  one construct meeting a token it also owns elsewhere. The last is the spelling the
  language's own precedence error recommends.

### Notes

- Everything here is pinned on all three engines. The release was preceded by a
  cross-feature sweep — dict literals inside match arms, UFCS inside interpolation,
  block comments between match arms, ranges beside guards — with zero divergences.
- The HTTP client's remaining gaps are known and specified rather than half-built: a
  cookie jar, method-preserving redirects (which RFC 10008 requires for QUERY),
  per-request timeouts, and case-insensitive header lookup. The QUERY method itself
  already works through `http_request`, verified end to end.

## v0.2.7 — 2026-08-19

**The stabilization release.** Before opening the next feature tier, everything the
last six releases added was put under an adversarial sweep: six agents, ~450 probe
programs, each run on all three engines and against the released v0.2.6 binary. This
release is what the sweep found — two criticals in v0.2.6's own `let` widening, three
silent-wrong-gradient shapes in the autodiff tape, a nondeterministic join, and three
ways `helix test` could misreport what it ran. No new features; every fix is pinned.

### Fixed — the JIT

- **A `let` in a float reduce body no longer aborts the process or silently prints
  `inf`.** v0.2.6 admitted `let` into the kernel's eligibility analyses and its codegen
  but not into the predicate that decides whether a kernel is built with its *poison
  cell* — the mechanism that carries a raised error out of a compiled loop. So `let`
  bodies were built without one. A user function called in a binding's initializer
  (`let d = sq(i * 1.0) in a + d`) then reached codegen that cannot represent it and
  aborted: exit 134, uncatchable by `try`, where both interpreters print `14.0`. A
  division by zero under a `let` was worse — the JIT printed `inf` and exited 0 where
  both interpreters raise `division by zero` and exit 1: a wrong answer *and* a wrong
  exit code from the flagship engine. Both were the same missing case, which now
  recurses into every binding initializer and the body. The kernel still engages
  (~30× on the 200k×64 correlation shape); the poison cell costs nothing measurable.

### Fixed — autodiff

- **A tracked value refuses mismatched shapes exactly where a plain one does.**
  `variable(tensor([1.0, 2.0])) + tensor([1.0, 2.0, 3.0])` silently returned the left
  operand unchanged — no addition performed, no error raised — and asking for a
  gradient through it aborted the process (exit 134, uncatchable) inside the backward
  pass. Both symptoms were one defensive fallback answering for a user's shape mistake;
  the tape now raises the same `cannot broadcast tensors of shape [2] and [3]` the
  plain expression raises. Legitimate broadcasting (the bias-add shape) is unaffected.
- **A tracked exponent is refused rather than silently frozen.** `gradient(2.0 ** x, x)`
  returned `0.0` — the exponent was read as a constant and dropped from the graph —
  where the true derivative is `2^x · ln 2`. It now reports "a tracked value can only be
  raised to a constant scalar power", the error already documented for tensor
  exponents. A constant exponent (`x ** 2.0`) is unaffected; a differentiable `a ** b`
  is a feature, not a silent zero.
- **A variable that does not feed the loss has gradient zero.** The backward pass zeroes
  only the nodes the loss reaches, so a variable left over from an earlier `gradient(…)`
  call reported *that* call's accumulation instead: in a training loop, one parameter's
  gradient could be another loss's. Nodes now carry the identity of the pass that
  touched them, and a variable from any other pass reads zero.
- **`gradient(x ** 0, x)` is `0.0` at `x = 0`** (was `NaN`).

### Fixed — DataFrames

- **Grouped aggregation after a join is deterministic.** Join output order was decided
  per plan execution, and `.column()` re-executes the plan — so two column reads of one
  grouped-after-join frame could pair keys from one ordering with values from another,
  at exit 0, with no warning. In the sweep's 500-group probe roughly 490 of 500 rows
  silently mispaired; even a two-group frame tore in 16 of 40 runs. Joins now pin
  reading order, the same guarantee `.sort()` already makes. The `.sort(key)` and
  `.cache()` workarounds are no longer needed.

### Fixed — `helix test`

- **The file walk terminates on symlinked directories.** A directory symlink pointing
  into its own tree made the runner count one test file 41 times and report success;
  two such links made it recurse until killed. Both collectors now share one walker
  that remembers where it has been.
- **Overlapping roots count each test once.** `helix test dir dir/module.helix` re-ran
  and re-counted that module's doc examples, reporting more passing tests than exist.
  Doc sources are now gathered across all roots and deduplicated by canonical path, as
  the test-file list already was — that list's adjacent-only deduplication had missed
  interleaved walks and differently-spelled paths too.
- **A doc example whose last line is `print(…)` can pass.** The harness wraps an
  example's final line in `print(…)` when the example documents output, so one that
  already printed emitted its value and then `()`. The failure report showed
  `expected: 3` against `got: 3` — identical to the eye, the real difference invisible.

### Notes

- Every fix above is pinned by tests that run on all three engines. The sweep's
  remaining findings are recorded in [`docs/dx-plan.md`](docs/dx-plan.md) with their
  mechanisms rather than fixed in haste: a grouped `i64` sum that wraps where the array
  path promotes, the frame/array disagreement on sorting `missing`, backend error text
  that names no Helix concept, and two name-resolution gaps around imported module
  names.
- The sweep's four other agents found no cross-engine divergence at all across 126
  fold fast-path programs, 87 CLI-surface programs, and the aggregate/eligibility
  families — the three-engine oracle held everywhere it was not explicitly broken by
  the two defects above.

## v0.2.6 — 2026-08-16

The reduce-eligibility family completed, and the autodiff aggregates closed.

### Performance

- **`let` in a float reduce body takes the JIT kernel** — the last live reduce trap
  (~19–23× in the field, measured 33× kernel engagement here: 51 ms vs 1,706 ms
  interpreted on the 200k×64-tap correlation shape). The old guidance — "write the
  subexpression twice; the native loop with a redundant op beats the interpreted one
  with CSE" — is dead: bindings are scoped in the analyses and the codegen exactly as
  the walker scopes them, sequential visibility included, with nested shadowing
  restoring on scope exit. Rebinding the accumulator or counter, and indices that
  mention a local, decline to the general path and stay bit-identical.

### Added

- **`.mean()` and `.product()` carry gradients** on arrays of tracked values, joining
  v0.2.5's `.sum()` — `mean` differentiates as fold-add over a divide (gradient exactly
  1/n), `product` accumulates repeated-element gradients (`[a, b, a]` gives d/da = 2ab).
  `.max()`/`.min()` remain unsupported on tracked arrays deliberately: a tie's
  subgradient needs a design decision, not a guess.
- **Pinned: `variable(tensor(…))` differentiates through `matmul`** — tensor-aware
  autodiff has existed in the tape and is now under test on all three engines
  (`gradient(w.matmul(w).sum(), w)` returns the analytic gradient exactly). A trainable
  layer's forward pass can be a real BLAS `matmul` when its parameters are created as a
  tensor variable. The scalars→tensor bridge (`tensor([w, …])` from tracked scalars)
  remains open.

## v0.2.5 — 2026-08-16

Two field reports from the llm/nn library builds, answered.

### Performance

- **A reduce's initial value may be any expression** — the last silent JIT cliff. A
  non-literal init (`reduce(a0, …)`, the natural ODE-integrator spelling) never
  compiled: identical body, identical answer, 21–53× slower. Four compile-time gates
  required a `Float` *literal* as a stand-in for a check the dispatch already makes on
  the runtime value — so a non-literal init now enters the float kernel family and the
  runtime decides. Parameter init at 100M iterations: **3,117 → 64 ms**. An init that
  turns out to be an `Int` falls back to the bytecode loop with a bit-identical answer.

### Added

- **The autodiff surface the nn library needed**: `sin`, `cos`, and `abs` gain
  derivative arms on the tape (`abs` uses the same subgradient convention at its kink
  that `relu` always has); `.sum()` on an array of tracked values folds on the tape, so
  the two spellings of a sum carry gradients alike instead of silently forking by
  capability; and `to_array(tensor)` flattens **natively** (row-major) — it was
  Python-gated, which put a feature wall exactly between the BLAS tensor path and the
  autodiff tape on a stock binary.

## v0.2.4 — 2026-08-16

**The linear-accumulation release** — [ADR 0029](docs/adr/0029-linear-accumulation.md)
implemented in full: building a collection one element at a time in a fold is
amortized-linear **on every engine, for every spelling** — arrays, dicts, and strings —
or it declines to the copy path and stays correct. The v0.2.3 field report measured the
released walker at the O(n²) signature (×2.1 → ×2.8 → ×5.2 ratios); this release is the
answer.

### Performance

- **The tree-walker's fold is linear** — the last engine with the append wall. The VM's
  take-append-store discipline transplanted: 262k array appends **6,768 → 23 ms**
  (294×), 64k dict inserts **17,689 → 16 ms** (1,100×). The walker's reduce also stops
  boxing packed receivers up front just to iterate them.
- **The string-interpolation fold is linear on all three engines** — previously
  quadratic everywhere (`13.6–14.6×` per 4×n; no engine had a fast path). The new
  `AppendStrIntoLocal` op and its walker twin render the tail first, so every fallible
  step happens with the accumulator untouched; a non-string init still formats exactly
  as before; a format spec on the accumulator hole still takes the general path (it
  re-pads, so append would be wrong); growth is fallible (a refused reservation reports
  instead of aborting). After: 4×n costs ~1.8×.
- **Duplicate fold binders `(a, a)` never take the fast paths** (shipped as a
  correctness guard in v0.2.2; the fast paths added here inherit it).

Every fix is pinned two ways: a complexity-class test (n vs 4n in one process — a
quadratic regression fails the gate rather than waiting for a field report) and a
semantics table byte-identical across all three engines *and* against the previous
release — self-referencing arguments, shared inits, scan's snapshot history, mid-fold
error restore, and the string fold's format-spec and non-string-init edges.

## v0.2.3 — 2026-08-15

Two fixes, both found by re-verifying v0.2.2's claims against the installed release —
the survivors of that audit.

### Fixed

- **The module-namespace guard now reaches string interpolation.** v0.2.2 fixed
  `print(mod.position(…))` but `emit("{mod.position(…)}")` still failed for all seven
  comprehension-lowered names — and the interpolated form is the idiomatic print, so the
  fix had missed exactly where users hit the bug first. An interpolation hole is parsed
  by a fresh parser; the import-namespace set now rides into it the same way function
  signatures always did.
- **`helix fmt` no longer indents a column-0 comment into the previous function's
  body.** Trigger: a nested lambda wrapped across lines — its closing brackets sit at
  line *end*, and the indent tracker only unwound *leading* closers, leaving a dead
  step for the next flush-left line to inherit. Dead steps are now discarded per line;
  all 54 example files still reformat to byte-identity.

## v0.2.2 — 2026-08-15

**The discoverability release** — plus the two deepest performance fixes since the
append wall, and four wrong answers removed. Everything below was driven by field
reports from the language's heaviest users (the #19 review and the physics-library
build); every fix is pinned by a test confirmed to fail on v0.2.1 first.

### Fixed

- **`helix test a.helix b.helix` ran only the first file, silently** ("running 1 test
  file") while `helix check` accepted many — anyone verifying two modules in one command
  believed both passed. Every path argument is now a root; files and directories mix.
- **Duplicate fold binders `(a, a)` diverged across engines**: the accumulator fast path
  matched by binder name, so `[[1],[2]].reduce([], (a, a) => a.concat([9]))` answered
  `[9, 9]` on the VM/JIT and `[2, 9]` on the walker — silent, exit 0. All engines now
  agree on the correct last-write-wins answer.
- **Qualified module calls beat comprehension sugar**: seven receiver-blind parse-time
  desugars (`position`, `sort_by`, `take_while`, `drop_while`, `min_by`, `max_by`,
  `zipmap`) intercepted `mymod.position(a, b, c, d)` and rejected a module's own 4-arg
  export. The parser now knows import namespaces; methods on them resolve to the module.
- **`unique()` on a large packed array could abort the process** — the general method
  dispatch boxed the whole buffer first (80M ints → 1.9 GB before one comparison). Now a
  packed fast path with fallible growth: refuses cleanly where memory genuinely runs out,
  and is 15–25% faster where it doesn't.
- **`helix test <file>` on a documented module** now answers what the directory run
  answers, instead of passing its examples and failing the file for asserting nothing in
  one output.

### Performance

- **The i64 map kernel admits affine indices**: `a[2*i]` 486 → 27 ms at 10M (16×), the
  3-point stencil `a[i] + a[i+1] + a[i+2]` 1871 → 31 ms at 20M (**50×**), measured
  against the v0.2.1 release binary. Values bit-identical on all three engines,
  including out-of-bounds and negative-index behaviour.
- **Array-of-strings accumulation is linear**: the fold spelling
  `lines.reduce([], (acc, s) => acc.concat([s]))` went **235.8 s → 61 ms** at 256k
  pieces (a `Values` arm in `concat_in_place`, guarded by a non-numeric witness that
  makes representation change impossible by construction).

### Added

- **`helix.toml` is a real manifest**: `description`, `authors`, `license`,
  `repository`, `keywords` — and `helix = ">=X.Y.Z"`, an **enforced toolchain floor**:
  an older binary opening the project reports *"this project requires Helix >= X, and
  this binary is Y"* once, instead of failing sixty confusing ways on unknown syntax.
  `version` must be comparable MAJOR.MINOR.PATCH. `helix new` writes the full template.
- **`helix describe` carries signatures** derived by probing the checker's own tables —
  accepted arities and per-arity return types (`round`: 1 arg → `Int`, 2 → `Float`),
  `null` where the checker genuinely does not constrain them, never fabricated.
- **`helix doc <name>` reverse lookup**: a method or builtin by name answers with every
  owner type, its effect, and an example receiver — the question users actually arrive
  with, previously answered by "error: unknown type".
- **`expect(k)` on Dict and Record** — the loud lookup: raises at the miss (with a
  one-edit did-you-mean over the collection's own keys) where `get`/`d[k]` keep
  ADR 0001's propagating `missing`.
- **Queries can name missingness**: `where(@v.is_missing())`, its `not` negation, and
  `drop_missing()` on DataFrames. `where(@v == missing)` still selects nothing — that is
  ADR 0001 semantics, and now there is an honest spelling for the real question.
- **The map at the door**: bare `helix` points at `helix help` / `helix doc` /
  `helix describe`; `AGENTS.md` at the repo root carries the commands, the three-engine
  correctness model, and the wrong-answer footguns.
- **Errors teach**: `prefix_sum` steers to `cumsum`/`scan`; an undefined name in a
  string hole explains interpolation and the `{{ }}` escape; `fn` inside `do {}` names
  the rule; unknown methods point at `helix doc <Type>` instead of dumping 79 names;
  reassigning `e`/`pi`/`inf` says what the constant is instead of suggesting a shadow.

## v0.2.1 — 2026-08-15

**The trust release.** No new features and no breaking API: seven wrong answers removed,
every one pinned by a regression test that was first confirmed to fail against the v0.2.0
binary, on all three engines. Three were release blockers found by a ~4,000-program stress
fleet within a day of v0.2.0 shipping; the deepest were on the DataFrame/Polars seam, not
in the language core.

### Fixed

- **Grouped `f64` `sum`/`mean` was nondeterministic** — the same pure expression could give
  a different answer across runs (and even twice within one program), because Polars'
  partitioned group-by merges intra-group values in a scheduling-dependent order. Grouped
  aggregations are now built so they structurally cannot take that path; the regression
  test evaluates the same query 30× in one process. This restores ADR 0020 reproducibility
  and un-blinds the three-engine differential oracle.
- **Grouped aggregations skipped `missing`** (ADR 0001 inverted): a group containing an
  unknown reported a number, and an all-`missing` group reported `sum` `0.0` —
  indistinguishable from a genuine zero. They now propagate `missing` per group, matching
  the array and whole-column paths. `count` is the deliberate exception: it counts rows,
  as `[1.0, missing].count()` always has.
- **Oversized collection materialization could abort the host** (SIGABRT, exit 134, not
  catchable) — the one ADR 0024 violation found. Three causes, all fixed: the tree-walker
  materialized packed arrays up front just to iterate them; the packed builder arms could
  overshoot their budget on `Vec` doubling; and the byte budget was checked only after the
  push. All three engines now refuse with the same catchable error, in the same words.
- **`.sum()`/`.mean()` returned `NaN` where IEEE-754 returns `±inf`** — Neumaier
  compensation went non-finite with the running sum. The compensation (and its accuracy on
  finite data) is kept; the non-finite case now answers what python3 and NumPy answer.
- **`erf` was the only math builtin not computed to double precision** (~1.4e-7 absolute
  error, discontinuous at 0, unbounded relative error near 0). It now matches python3's
  `math.erf` bit-for-bit across the tested grid, and `normal_cdf` routes through `erfc`,
  fixing the left tail — the p-value case — which no `erf` accuracy alone could fix.
- **DNA IUPAC arithmetic**: `gc_content` counted `S` ("G or C" — GC by definition) as
  non-GC, so `dna("S")` read 0.0 and `dna("GCS")` read *lower* than `dna("GC")`. The
  policy, now uniform and documented: a base participates iff its GC-ness is certain
  (`S` is GC, `W` is not; `N R Y K M B D H V` are excluded from numerator and denominator
  alike — the rule `N` always had). A sequence with no classifiable base answers `missing`,
  never a fabricated `0.0`, and that propagates through `mean_gc`, which had been averaging
  all-`N` sequences in as `0.0`.
- **`try 1 + 1` produced a true but useless error** ("got a Record", with no record in
  sight — it parses as `(try 1) + 1`). The error now explains that `try` binds tighter
  than the operator and shows the parenthesized fix. Ordinary record operands keep the
  ordinary message.

### Hardened

- **`sort` tie order is now a contract, not an accident**: DataFrame `sort` pins
  `maintain_order`. No misbehaviour was ever reproduced (300k rows, int and string keys),
  but unstable tie order is unspecified and every `.column()` re-executes the lazy plan,
  so the pin makes the stability ADR 0020 assumes survive any Polars upgrade.

## v0.2.0 — 2026-08-13

**The consistency release.** Every breaking change this project has decided is in this one
version, so upgrading is one deliberate step instead of a drip. Each break below names the
ADR that argued it; each was landed with its blast radius measured first, and the ordering
changes are specified cell-by-cell by `tests/ordering_matrix.rs` (247 pinned
expression/answer pairs, identical on all three engines).

The theme is one sentence: **a library author must be able to write correct code without
knowing anything about the caller** — not their schema, not their manifest, not which
builtins a future Helix adds.

### Breaking

- **One order, one domain** ([ADR 0025](docs/adr/0025-ordering.md), all four questions).
  `sort`, `argsort`, `sort_by`, `min`/`max`, `min_by`/`max_by` and `argmin`/`argmax` now
  agree about what can be ordered and what `missing` means:
  - `argsort` (and therefore `sort_by`) adopts `sort`'s policy: an array with `missing`
    **errors** with the same message and hint, and DNA orders. Before, `xs.sort()` and
    `xs.sort_by(it)` disagreed with each other.
  - `min`/`max` widen to `sort`'s type domain: strings and DNA now reduce
    (`["b","a"].min()` is `"a"`), so `min() == sort().first()` holds everywhere. The
    reduction policy is unchanged: `missing`/NaN still propagate as `missing`.
  - `min_by`/`max_by` and the **method** `argmin`/`argmax` adopt the reduction policy:
    `missing`/NaN propagate instead of raising leaked internals (`` `if` condition is
    `missing` ``, `index 0 is out of bounds`), and the empty array gets a named error.
    `xs.argmax()` and `argmax(xs)` now give the same answer to the same question.
  - Signed-zero ties on `argmin`/`argmax`/`min_by` remain IEEE first-wins, now documented
    with runnable examples in `examples/language/ordering.helix` (the gate executes them
    on all three engines, so the documentation cannot drift).
- **In a DataFrame query, a bare name means the binding, not the caller's column**
  ([ADR 0028](docs/adr/0028-query-name-resolution.md)). `fn above(frame, cutoff) =
  frame.where(@value > cutoff)` no longer changes meaning when the caller's data happens
  to have a `cutoff` column. `@name` still pins the column side; a bare name with no
  binding in scope is still a column. This was the last known silent wrong answer.
- **A top-level `fn` is file-scoped** ([ADR 0027](docs/adr/0027-builtin-shadowing.md)).
  It is callable above its own definition, and shadowing a builtin is retroactive for the
  whole file — a name means one thing per file. This removed a silent three-engine
  divergence, and it is what lets future Helix releases add builtins without breaking
  published libraries that already use those names.

### Fixed

- **A consumer's dependency can no longer capture a library's private import.** A file's
  own sibling now wins over a dependency key, so installing a package named `helpers`
  cannot rewire another library's `import helpers`. The failure was silent (exit 0,
  `check` ok, all three engines agreeing) and is now pinned by the one test able to see it.
- **Growing an array or dict in a fold is linear.** 256k `acc.concat([x])` appends went
  6.49s → 0.04s (158×) and 4M run in 0.25s; `Dict.insert` follows. The accumulator is
  taken, extended and stored as one VM instruction, so nothing observes it mid-move.
- **A `reduce` body can call a user function with a float signature** — 1.84s → 0.05s at
  n=20M (the kernel used to decline outright). A `/0` or NaN comparison inside the callee
  still raises the exact interpreter error.
- Mutual recursion and forward references compile and run on all three engines, with
  mutual tail calls constant-space to 1M frames.

### Added

- `raise(message[, help])` — a library can reject an argument in its own words, with a
  `help:` line, instead of `assertion failed: …`. Caught by `try` like any error.
- `source_path(rel)` — resolves against the directory of the file the call is written in,
  so a package can ship and read its own data regardless of the process working directory.
- `chars()` — a string as an array of characters (Unicode scalars). The previous
  spelling, `replace("", "\t").split("\t")`, was both undiscoverable and wrong.
- `helix test` runs your `## >>>` doc examples on all three engines and fails a test file
  that asserts nothing. A facade re-export (`export f = lib.f`) keeps defaults and named
  arguments. `helix verify` hashes a dependency's data files, not just its source.

### Upgrading

If your code never shadows a builtin, never names a `min_by` parameter after a DataFrame
column, and never sorts arrays containing `missing`, v0.1.1 programs run unchanged. The
repository's own corpus, examples and benchmarks needed **zero** edits beyond the ordering
test matrix itself; the one fixture that deliberately pinned the old shadowing behaviour
was regenerated.

## v0.1.1 — 2026-08-11

**The first installable release.** `v0.1.0` is published but nobody can install from it, and
this release exists to replace it rather than to add anything.

### What was wrong with v0.1.0

Probed against the live release: `helix-x86_64-unknown-linux-gnu.tar.gz` → **404**,
`SHA256SUMS` → **404**. Four of six platforms uploaded, and the installers refuse to install
what they cannot verify, so even those four were unreachable. Three independent causes:

- The profile-guided Linux build died in its training step on
  `bench/crosslang/b3_groupby.helix`, which still called `io.read_csv(…)` — a spelling
  ADR-0017 removed. **Nine** benchmark programs had rotted the same way; nothing in CI ever
  compiled them, because every gate the project had *runs* its programs and these need a
  250 MB generated fixture first.
- Six build jobs each called `action-gh-release`, which creates-if-missing, so six of them
  raced to create one release. The musl job died mid-upload.
- The checksum job was skipped for a failed dependency, and the incomplete release went
  public anyway.

### Fixed

- **The release is built as a draft and published last.** One job creates it, every build
  uploads into it, and publishing is gated on all six platforms plus `SHA256SUMS` being
  present. A failure now leaves an invisible, re-runnable draft instead of a broken public
  artifact.
- **Every build smoke-tests the binary it produced** where the runner can execute it, and
  the musl job asserts the artifact is genuinely static — the one machine that would
  otherwise discover it is the air-gapped one with no way to fix it.
- **`workflow_dispatch` takes a `dry_run` input** (default on) that builds and smoke-tests
  all six platforms while writing nothing, so the pipeline can be checked without publishing
  something to check it with.
- The nine stale benchmark programs are repaired and verified by running them.

### New

- **`helix check <script>…`** — type-check without running or writing anything. Takes many
  paths in one process; `scripts/checkall.sh` covers all 85 tracked programs in ~0.03 s and
  runs in CI, which is what closes the gap that let the benchmarks rot. It never executes
  the program, and it is honestly *only* a type check: code that checks clean can still fail
  at run time.

### Toolchain and hygiene

- All five CI jobs now block. Clippy is at zero warnings with `-D warnings` (no `#[allow]`
  suppressions); MSRV 1.96 is verified rather than asserted; `cargo audit` is green, with two
  advisories fixed by upgrade (crossbeam-epoch, quinn-proto) and four that have no reachable
  fix documented in `.cargo/audit.toml`, each with the crate that blocks it.
- `CONTRIBUTING.md` added.
- The Docker build context went from 1.3 GB to 9 MB — `.dockerignore` was not excluding
  `website/node_modules`, `website/.next` or the generated benchmark fixtures.

**No language changes.** A `v0.1.0` program runs identically on `v0.1.1`.

## v0.1.0 — 2026-08-11

First tagged release. Incomplete — see above; use `v0.1.1`.
