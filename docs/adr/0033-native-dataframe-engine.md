# ADR 0033 — A native DataFrame engine: replace polars, staged, with polars as the oracle

- **Status:** **Accepted 2026-08-23; Stage 0 implemented** (commit `5c203dc`) — the
  frozen frame format lives in `src/framefmt.rs` (the module doc IS the spec; its
  first test asserts the spec's example byte-for-byte), `collect_string` left the
  seam, `POLARS_FMT_*` no longer reaches program output, three engines verified
  byte-identical. **Stage 1 implemented** the same day: `src/backend/native/`
  (ten single-purpose files), the full DataHandle surface through the
  interpreter's own scalar kernel per ADR 0034, 12/12 differential tests against
  the polars oracle (the harness caught two real ordering divergences on first
  contact — keep-last row order, right-join column layout — both fixed), the
  dataframes examples byte-identical across engines modulo the decided deltas,
  and the appliance profile now ships WORKING frames at 9.3 MB. First honest
  perf probe (1M rows, CSV-heavy, min of 3): native 156 ms vs polars 77 ms —
  parse-dominated, the Stage 3 item, outputs byte-identical. **Stage 2 implemented**
  (commit `fc1bd3e`): native parquet via the apache crate, no arrow — cross-engine
  compatible both directions (zstd 3, the polars default), foreign dtypes as text
  per ADR 0034's totality, nested refused at the root; the appliance does full
  frame IO at 11.3 MB. **Stage 3 implemented** 2026-08-23 (commits `85f16de` through
  `26b9d88`): parallel CSV both directions, dictionary-encoded string columns,
  hand-rolled parquet pages, lazy per-column decode with page-level predicate
  pushdown — native crossed over on the 1M anchor and now beats the polars backend
  on all 16 verbs of the 5M-row matrix (one machine, one workload, min of 3, every
  result cell compared against the polars oracle). **Stage 4 implemented**
  2026-08-31: the default flipped. `default` and `bio` now pull `native-df`;
  polars stays behind the `dataframes` feature **as the oracle only**. The
  full-featured binary went 120 MB → 31 MB (stripped 77 → 20), crates compiled
  1,566 → 192, startup 4.9 → 2.96 ms like-for-like (2.5 ms is the APPLIANCE
  build — quoting it against the old full-featured binary reported two changes
  as one, which is why a field build measuring ~4 ms could not reproduce it). See "Stage 4" below for what the flip cost
  and what it exposed.
- **Date:** 2026-08-23
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0012](0012-dataframe-backend-seam.md) (the seam that makes this
  possible — "go homegrown later" was its stated purpose), [ADR 0032]
  (0032-appliance-profile.md) (the gate pattern Stage 1 rides), [ADR 0001/0025]
  (missing/ordering doctrine the native engine implements directly),
  [docs/binary-size.md](../binary-size.md) (the unfixable-tail proof).

## Context: what the audit established

- **The seam is airtight and already survived production.** `polars` appears in six
  files; the trait surface is 18 methods + 3 free functions, all shared by both
  engines (no second copy to drift). ADR 0032 shipped "compile with zero backends."
- **Half of backend/polars.rs is armor, and that is the real maintenance cost.**
  The API-churn argument is PROSPECTIVE, not historical — the repo has never paid a
  polars upgrade (pinned 0.54 since the seam landed). What the log actually shows is
  five commits defending against the engine's runtime behavior: nondeterministic
  group order (e079749), join-order coin-flip silently mispairing ~490/500 rows
  (62716f1), a fast-count answering 1 with exit 0 on an unparseable file (cd05369),
  `read_csv(f).where(1)` aborting the process (c243040 — "ADR 0024 was false in
  production"), parity divergences (71b94bb). Helix's determinism and
  missing-propagation doctrines run against polars' grain; an eager,
  single-threaded-per-shard native engine makes that whole failure class
  structurally impossible.
- **The tail is unfixable on 0.54.4.** object_store -> reqwest -> hyper -> tokio
  enters via `polars-error` unconditionally (~35-39 MB of the default binary, 192
  lock crates). The upstream fix is post-0.54, purchasable only by paying the
  unstable-API upgrade. There is no free "keep" branch: pay churn to shrink the
  tail, or pay the swap to delete it.
- **Nothing pins polars' table text.** `shape: (` appears in zero tests, examples,
  docs, or corpus fixtures; the interactive rich-print path is ALREADY Helix-owned
  (render.rs builds its own table from seam methods). The scariest surface —
  matching polars' Display byte-for-byte — is optional.
- **The oracle harness already runs in CI.** 143 corpus fixtures pinned
  engine-identical, dfcheck.sh (built to catch row-order nondeterminism), vmparity,
  two differential fuzzers.

  > **Correction (v0.6.0).** `dfcheck.sh` is named here as standing evidence and was not
  > evidence: the path it ran had moved, so it diffed three copies of "no such file" and
  > reported them identical. It is deleted. `scripts/dfdiff.sh` replaces it and runs every
  > tracked `.helix` under both backends — the first run found the divergences
  > [ADR 0036](0036-one-semantics.md) closes. Read every "already runs in CI" claim in this
  > ADR against that. The `.expected` files ARE frozen polars output. Both
  backends can coexist in a dev binary (cross-backend join/vstack already errors
  cleanly via `as_any`), so side-by-side property tests need no seam changes.

## Decision (proposed): replace-staged

Polars becomes the development oracle for its own replacement. Each stage ships
alone; the default flip is one release with polars retained behind a feature for a
rollback window.

**Stage 0 — annex piped frame rendering (before any engine code).** Extend
render.rs's engine-agnostic table path to non-TTY `print(df)` and interpolation,
with a frozen Helix format spec (Helix float formatting, the `missing` token, fixed
truncation, no `POLARS_FMT_*` env sensitivity — today's env porosity violates
render.rs's own byte-identity doctrine). A deliberate, versioned, release-noted
format change, decoupled from the engine change, landed while polars is still the
engine.

**Stage 1 — `NativeFrame` as the appliance profile's engine (pure addition).**
`enum NativeCol {I64, F64, Str, Bool}` + validity masks; `build_frame`; every
in-memory trait method (filter/select/with_columns/sort/join/group_agg/unique_by/
head/vstack-with-eager-dtype-check/cache=identity/...); full ColExpr eval — with
the `%` truncated-vs-euclid and `/0` error-vs-null policies decided in writing
FIRST; `read_csv` on the csv crate with a documented inference policy; `write_csv`;
`collect_string` = the Stage 0 renderer; parquet answers the ADR 0032 gated error.
Feature `native-df`, composable into `appliance` (~14-15 MB with frames). Appliance
has no frames today, so the regression surface is zero. Acceptance gate: full
corpus + dfcheck + fuzzers against the polars-frozen `.expected` files, plus a
dual-backend binary running side-by-side property comparisons; every divergence
fixed or written down as a decided policy delta.

> **Correction (v0.6.0).** This gate was declared met while `dfcheck` was inert (see the
> correction above) and while "every divergence fixed or written down" was false —
> sixteen were live, five recorded nowhere. `scripts/dfdiff.sh` is what the clause
> intended and now enforces it, at 0 undeclared divergences ([ADR
> 0036](0036-one-semantics.md)).

**Stage 2** — apache `parquet` (no arrow glue) for the four dtypes +
foreign-dtype-to-string fallback + footer-metadata count. **Stage 3** — parallel
CSV parse (rayon chunks over memchr record splits) + filter kernels, measured
against the 113 MB crosslang anchor under the house measurement discipline.
**Stage 4** — flip the default; polars retreats to the `python` feature (the
pyo3-polars Arrow bridge keeps it), then out entirely if the bridge moves to plain
Arrow interchange.

## Stage 4 — the flip, and what it exposed

**The oracle outlives the default it replaced.** Retiring polars from the product
is not retiring it from the evidence: an engine cannot be its own proof, and
`scripts/dfdiff.sh` running every tracked program under both engines is what says
the replacement means the same thing. So `dataframes` stays, in CI and in dev
builds, and only stops being the default.

Three consequences that were not obvious until the flip:

- **The feature that adds a SECOND engine inverted.** Dual builds are
  `--features dataframes` now, not `--features native-df`. `scripts/gate.sh`,
  `scripts/dfdiff.sh` and CI all name it.
- **A dual build must default to native too.** It defaulted to polars, which would
  have had every developer and every CI job exercising the oracle while every user
  ran the shipped engine. `polars_selected()` replaced `native_selected()` so the
  fallthrough — every run that says nothing — lands on what ships.
- **`dfdiff`'s dual-ness guard went vacuous and had to be re-pointed.** It proved a
  binary carried two engines by checking `HELIX_DF_ENGINE=native` was accepted.
  Native now ships everywhere, so that passed for a single-engine binary and the
  harness would have compared native against itself: 129 programs, 0 divergences,
  zero information — the same failure `dfcheck.sh` once shipped by diffing three
  copies of "no such file". It probes for `polars` now, and the probe was verified
  to fire.

### Three undeclared divergences, and why nothing had seen them

`dfdiff-allow.txt` is empty, so by this ADR's own rules each was a defect rather
than a decision:

1. **Native refused ragged CSVs** while polars padded short rows with missing and
   truncated long ones. `tests/cli.rs` records the lenient behaviour as policy —
   "a deliberate decision that rejects files real pipelines emit, and it belongs
   in an ADR, not a diff" — so native was corrected to match. An engine swap must
   not change language semantics; changing CSV strictness is its own decision.
2. **Native's join bypassed `validate_join_keys`**, which had exactly ONE caller
   (`backend/polars.rs`), so a bad key lost *which frame* was missing it.
3. **`#[cfg_attr(not(feature = "dataframes"), allow(dead_code))]` was hiding #2** —
   a native-only build compiled the shared validator as dead code without a word.
   An `allow` that silences a warning also silences the question it was asking.

All three were invisible for one reason: **no corpus program read a CSV at all**,
and none joined on a bad key. 157 corpus files, zero `read_csv`. A differential is
evidence only for what its corpus exercises. Five programs now cover the gap —
`df_ragged_csv`, `df_join_bad_key`, `df_unique_keys`, `df_group_keys`,
`df_join_dense_edges` — taking dfdiff from 129 to 134. `dfdiff.sh` uses
`git ls-files`, so a new corpus program is invisible to it until `git add`.

### Performance: dense direct addressing

The remaining gaps all had one cause and one fix. **A hash table exists to map an
arbitrary key onto a dense slot; when the key is already a dense integer that map
is the identity and the table is redundant work.** The columnar format supplies
that for free — `Col::Str` codes are dense in `[0, dict.len())` by construction, so
the distinct set is *already computed*, and an `I64` column's range is one scan
away. The join's `Str` branch had done this since Stage 3; it simply had not been
generalized. `dense_domain`/`dense_slot` are now one definition shared by `unique`,
whole-row `unique` and `group`, because three copies of that arithmetic are three
chances to disagree about key identity — a silent wrong answer, not a crash.

Two more came from measuring rather than guessing, after two wrong guesses on join:
contiguous gathers are memcpys (`head`/`tail`/`slice` and the identity index a
dimension join produces), and the join's `pairs` vector existed only to be split
into the two index columns everything consumed — 32 bytes a row to express two
16-byte columns, ~25 MB before a single column was gathered.

At 1.6M rows on materialised frames with every output consumed, min-of-7
(polars → native): `group` 20.7 → **5.2 ms** (4.00×), `join` 84.6 → **29.9**
(2.82×), `unique(col)` 9.0 → **3.2** (2.81×), `with` 49.0 → **19.8** (2.47×),
`sort` 74.5 → **38.1** (1.95×), `where` 26.0 → **13.6** (1.92×), `unique`
33.2 → **23.4** (1.42×). Native wins every verb.

### The measurement rule this stage cost four wrong answers to learn

**A lazy engine's fast number is usually a refusal.** `read_csv(p).count()` is
answered without parsing a field; `join(dim, @k).count()` on a one-to-one join is
answered without joining; `sort(@x).count()` needs no order and is dropped. Timed
that way polars read 0.11 ms for a sort that costs 74 ms and 5.7 ms for a join that
costs 84 ms — turning two native wins into published losses. The tell is always the
same: **sub-linear growth**. Polars' "join" grew 1.12× for 4× the rows, which no
join does. Every program in `bench/df/` now ends in `.column(...)`, and `read.helix`
is kept as the deliberate probe of exactly this shortcut, with `parse.helix` as its
honest counterpart.

### What Stage 4 did not do

`--features python` still requires `pyo3-polars` — two call sites in
`src/python.rs` exchanging a `PyDataFrame`. It is the one configuration where
polars reaches a shipped binary. The replacement is the Arrow PyCapsule interface,
which would also buy interop with pandas, pyarrow and duckdb rather than polars
specifically; it is not taken here because it needs either the arrow stack this
ADR's Stage 2 deliberately avoided, or a hand-written C Data Interface, in exchange
for removing a dependency from a feature nobody gets unless they ask for it.

## Honest costs, stated up front

- **4-6k LOC and a multi-week campaign** (the audits' 2.2-3.6k LOC is the code; the
  type-coercion matrix, CSV-inference policy corners, and differential triage are
  the schedule). Still an order of magnitude below polars-parquet alone (48k LOC),
  and every corner has an executable oracle.
- **Big-CSV ingest gets slower first** (csv crate is single-threaded; expect 3-5x
  on the 113 MB anchor until Stage 3), filter kernels 3-4x (measured in dfbench).
  Offsetting wins the same users feel: today every `.column()` RE-EXECUTES the
  whole lazy plan unless the user knows to `.cache()`; the honesty-fixed `count()`
  costs 280 ms per call on the anchor — eager makes both free, and group_agg
  measured 4-25x faster on dense keys. Out-of-core frames are lost — the flagship
  bio path never had them (five of seven genomics readers already build eager
  ColData).
- **Stage 0 is a real (minor) breaking change** for external scripts that parse
  piped table text; released with notes, not slipped in.

## What flips this back to keep-polars

1. Stage 1's differential campaign exceeds ~25-30 distinct POLICY decisions (not
   bugs) — the bounded-semantics premise fails, and ADR 0012's insurance was the
   final answer.
2. A polars release with a stability commitment AND an optional object_store that
   puts the default binary under ~25 MB — re-measure before Stage 2.
3. After Stage 3, `read_csv` + filter on the anchor is still >2x slower than
   polars under the house measurement discipline — the hybrid (native-df in
   appliance only) becomes terminal instead.
4. Zero-copy Helix-to-Python frames become a headline feature — pyo3-polars' Arrow
   path keeps polars in the builds that matter (it survives behind `python`).

## Addendum 2026-09-20 — the text builder moved to the seam, and hashes a cell once

Text is dictionary-encoded (Stage 3), and every string column is built by one hash-consing
builder. Two things changed about it, both found from outside the engine — in the PostgreSQL
driver's profile (ADR 0044's addendum of the same date).

**It lives at the seam** (`backend::strbuild`; the engine re-exports it and keeps `finish`).
A reader may not name an engine type, so a cell-at-a-time reader used to collect
`Vec<Option<String>>` for the engine to intern afterwards: a `String` allocated per CELL,
hashed, and — the text nearly always being in the dictionary already — thrown away. A reader
now interns as it reads and hands over `ColData::StrBuilt`; `ColData::IntValid`/`FloatValid`
are the numeric columns' own shape (values, validity alongside), handed over without the pass
`IntOpt` costs. The oracle builds the same columns from them.

**It hashes a cell once.** The index was std's `HashMap<DictKey, u32>`, which cannot look a
key up by `&str` and insert an owned one on a miss without hashing twice, and hashes every
key again when it grows. It is now a `hashbrown::HashTable` of codes that is handed the hash;
each entry's hash is kept beside the dictionary so growth re-reads no string. 76 → 52 ns a
distinct cell at 1 000 values, 94 → 49 at 100 000; a repeated value costs what it did. The
hash is still SipHash under a random key — a column's cells are data from strangers — and
`hashbrown` was already in every build, so this added an edge to the lock and no crate.

## Addendum 2026-09-22 — the readers' worker threads intern through the same builder

The addendum above named what it left. The parallel readers intern on worker threads, where
an `Rc` may not go, so each kept an interner of its own — std's `HashMap<String, u32>` beside
a `Vec<String>`. `read_csv` had one a chunk a column; `read_parquet` had the same thing three
times in one file. A distinct text was stored TWICE, hashed by `get`, by `insert` and by every
growth of the map, and then — at the splice, on the main thread, where nothing is parallel —
hashed a further time and copied a third, into the column. Parquet's splice was stranger: its
worker interns the WHOLE column, so the main thread was interning a dictionary that already
was one, to be handed back the codes 0, 1, 2, … it started with.

**One interner, generic over how a text is held.** `backend::strbuild::Interner<S>` is the
index alone — dictionary, cached hashes, `hashbrown::HashTable<u32>`, key — with `S` an
`Rc<String>` in a column and a plain `String` in a worker's chunk, which is `Send`.
`StrBuilder` is an `Interner<Rc<String>>` plus codes and validity, and compiles to what it
was: `push_str` and `push_rc` are the same 223 and 201 instructions before and after, one
alignment no-op apart.

**One key a read, so a text is hashed once.** Every chunk of a `read_csv` interns under the
same `RandomState`. The first chunk to reach the splice becomes the column's dictionary, table
and all — nothing is probed, so a file of one chunk (4 096 rows or fewer) never probes — and
each later chunk is looked up under the hashes its worker computed; a text the column had not
met is MOVED into its `Rc`, not copied. A hash is only as good as its key, and a lookup under
the wrong one does not fail: it MISSES, and interns a second copy — two codes for one text,
and a group-by that splits a group in two. So `absorb` hashes one text of each chunk again to
see that the chunk was made under its key, and hashes them all if it was not. Sharing the key
gives nothing away: it is as random, and as unobservable, across a read's tables as in one.
A parquet dictionary is simply wrapped where it lies.

One codegen unit and a fresh build on both sides, five interleaved rounds, the minimum of
each row, the load average under 0.9 before every round, and a checksum of every text column
that must agree before a number is read. The programs were scratch under `target/bench/` and
are not kept, so the inputs are written down: every row is `read_csv(path)` timed in-process
(the best of five to nine runs). `hi` is an `id` and four all-distinct text columns (one
holding a comma, so quoted); `lo` is five text columns of 40, 7, 3, 100 and 12 distinct
values; `mid` is an int, a float, text columns of 5 000, 20 000 and 50 000 distinct values,
and one of 1 000 with every tenth cell missing; `hi1m` is a million rows of an `id` and two
all-distinct text columns; `small` is 3 000 rows read a hundred times. The f90 rows are an
`id`, a distinct text column and a 40-value one. 200 000 rows unless said:

| ms | before | after | | `HELIX_THREADS=1` before | after | |
|---|--:|--:|--:|--:|--:|--:|
| `read_csv`, 4 distinct text columns | 90.29 | 50.30 | 0.56 | 163.67 | 93.82 | 0.57 |
| `read_csv`, text repeating across chunks, holes | 37.16 | 20.20 | 0.54 | 73.34 | 53.38 | 0.73 |
| `read_csv`, 5 low-cardinality text columns | 8.50 | 8.03 | 0.94 | 31.70 | 32.02 | 1.01 |
| `read_csv`, 1 000 000 rows x 2 distinct text columns | 269.61 | 163.10 | 0.60 | 614.72 | 283.43 | 0.46 |
| `read_csv`, 3 000 rows (one chunk), x 100 | 46.20 | 30.00 | 0.65 | 46.53 | 29.94 | 0.64 |
| f90: `read_csv`, distinct + 40-value text | 21.45 | 14.53 | 0.68 | 42.85 | 31.20 | 0.73 |
| f90: `read_parquet`, same columns | 26.60 | 10.82 | 0.41 | 25.30 | 10.84 | 0.43 |
| f90: `dataframe()`, same columns | 15.96 | 15.77 | 0.99 | 16.03 | 16.33 | 1.02 |
| f90: `to_dataframe` of 50 000 records | 12.02 | 12.12 | 1.01 | 12.35 | 12.40 | 1.00 |
| `read_gff`, 100 000 features (`push_str`) | 69.59 | 68.58 | 0.99 | | | |
| outer join on a text key (`push_rc`) | 22.45 | 22.50 | 1.00 | | | |

Peak RSS of the million-row read: 472 → 442 MB. The `dataframe()` row is this harness's
noise floor: no thread count reaches it and its code did not change, and it reads 0.99 and
1.02. A repeated cell costs what it did — one SipHash, one probe — which is the
low-cardinality row at one thread. What that read gains at six is in the splice, the one part
of it this change touched: a chunk's cells are rewritten in one `extend` where they were two
pushes each.

**Measured and declined: inlining the probe.** Between two builds of this change a column of
repeated text read 5% apart (30.7 against 32.3 ms a million cells), and the symbol table had
an explanation ready: hashbrown's `entry` — probe and insertion in one walk — is inlined only
while it has one caller, and the parquet reader had become `intern`'s second. So the probe
was split from the insertion and forced inline (`find`, then an out-of-line `insert_unique`):
repeated text 4.5% faster, a million rows of distinct text 1.3% slower, the second walk
showing in a table too big for the cache. Then the control: a build with the split probe for
cells and the single walk for the splice. Its per-cell code IS the fast build's — `Seg::push`
the same 392 instructions at the same address, the worker's row loop the same 622
instructions 48 bytes away — and it timed like the SLOW one; so does the engine before this
change, which shares none of that code. The 5% follows where a loop LANDED, not what is in
it. One walk and no attributes is what shipped, and a few percent between two builds is not
evidence of a design on this machine, one codegen unit on both sides or not: the control is
a build that holds the code still and moves something else.

What is left of a distinct-text read is the representation, not the interning: an
`Rc<String>` a text is two allocations made on one thread, and two frees when the frame is
dropped.

The splice had run in no test. Every CSV in the suite and in the corpus was a single chunk,
so a differential over the whole corpus had never spliced one. `csv_text_is_one_dictionary_
across_chunks` reads 20 000 rows — text that repeats across chunks, text that never repeats,
a column whose first texts arrive with a LATER chunk, the empty string, a comma, a quote —
and asks that every cell read back as written and that the dictionary hold each text once,
in first-seen order. It passes before this change as well as after: that is what it is for.
