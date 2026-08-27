# ADR 0038 — Database access: a query is a DataFrame, and injection is unrepresentable

- **Status:** **Accepted — Stage 1 (SQLite, read-only) implemented.** Stages 2–3
  (SQLite writes, PostgreSQL) proposed.
- **Date:** 2026-08-27
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0012 — DataFrame backend seam](0012-dataframe-backend-seam.md),
  [ADR 0021 — Capability sandbox](0021-capability-sandbox.md),
  [ADR 0032 — Appliance profile](0032-appliance-profile.md),
  [ADR 0033 — Native DataFrame engine](0033-native-dataframe-engine.md),
  [ADR 0037 — The scripting surface](0037-scripting-surface.md)

## Context

Helix reads CSV, Parquet, JSON, and six genomics formats. It could not read the place
most real data actually lives. A scientific pipeline that must export a table to CSV
before Helix can touch it has a gap where its input should be.

The question is not whether to add database access; it is what shape it takes, because
three of this project's existing decisions constrain it hard:

- **ADR 0012's seam.** DataFrame construction is backend-agnostic
  (`backend::build_frame` over `ColData`), and anything that builds a frame must go
  through it or the "engine types live in one file" property is lost.
- **ADR 0032's size budget.** The appliance profile is a deliberately small binary. A
  dependency that quietly costs tens of megabytes is not free to add.
- **ADR 0021's labels.** Every authority-bearing builtin carries an effect category, and
  a label that overstates what a verb does makes the audit log worse than no audit log.

## Decision

### D1 — A query returns a **DataFrame**.

```helix
users = sqlite_query("app.db", "select name, age from users where age > ?", [30])
print(users.where(@age > 40).group(@city).mean(@age))
```

Rows-of-records would arrive *next to* Helix's frame surface instead of joining it. A
`Df` plugs straight into `where` / `group` / `sort` / `join` / `write_csv` — everything a
`read_csv` result can do — which is the same decision the CSV and genomics readers already
made, for the same reason.

Construction goes through `backend::build_frame`, so this works on the polars backend and
the native one alike. **This is not a formality: the feature was first written as
`db = ["dataframes", …]`, copying `bio`, and that cost 63 MB** — because `dataframes`
means *polars*, and the appliance profile deliberately uses `native-df` instead. `bio`
was written when polars was the only backend. Measured: appliance + db via the seam is
14.0 MB; via `dataframes` it was 75.5 MB.

**SQLite is dynamically typed per value, not per column**, so a column's type is
discovered from its rows and widens Int → Float → Str, the order that loses least. A
column of only NULLs stays untyped and lands as an all-null String column rather than
guessing a type it never saw. `NULL` becomes `missing`, not a sentinel (ADR 0001).

### D2 — Parameters are values. There is no string-building form.

```helix
sqlite_query(db, "select * from users where name = ?", [name])
```

The safe form is the *only* form the API offers, which is the same call ADR 0037 D3 made
for subprocesses: injection is not discouraged, it is unrepresentable through this
surface. A parameter carrying the classic payload matches a user literally named
`x' or 1=1 --`, which is to say nothing — asserted in `tests/cli.rs`.

This does not stop someone assembling SQL with string interpolation and passing the
result. Nothing at this layer can. What it does is make the parameterised form the
shortest path, so the dangerous one is never also the convenient one.

### D3 — Read-only, so the capability label is the truth.

`sqlite_query` opens the database `SQLITE_OPEN_READ_ONLY` and is classified `fs-read`.

Those two facts are one decision. Opening a SQLite file can *create* it, and arbitrary
SQL can write to it, so a verb labelled `fs-read` that could execute `DELETE` would make
the ADR 0021 audit log state something false. Enforcing read-only at the connection is
what earns the label — `delete from users` is refused by SQLite itself, and the test
asserts that.

It has a second benefit that matters more day to day: a typo in the path **fails** instead
of silently creating an empty database, which is the failure mode that turns "why is my
table missing" into an afternoon.

Writing needs its own verb with its own `fs-write` label. That is Stage 2, not a flag on
this one.

### D4 — Feature-gated, with the body gated rather than the name.

`--features db`. In a build without it, `sqlite_query` still exists, still type-checks,
still appears in `helix describe` with its signature and effect — and running it says
`this build has no SQLite support … rebuild with --features db`. That is ADR 0032's
gate-the-body pattern, and the test asserts the *shape* in every build and the *behaviour*
only where the feature is on, because testing only the enabled half is how a gated verb
rots in the builds that lack it.

**Measured cost** (gate profile, stripped, appliance = `http + mimalloc + native-df`):

| build | size |
|---|---|
| appliance | 12,675,896 B |
| appliance + `db` | 14,661,832 B |
| **delta** | **1,985,936 B — 1.9 MB, 15%** |

SQLite is compiled from bundled C source, so the binary keeps its "no system dependency"
property: no `libsqlite3` to install, as with no BLAS and no OpenSSL.

A caution on measurement, because the first attempt was wrong: an unused dependency links
to **nothing**, so measuring before the code existed showed a 64-byte *decrease*. The
number above was taken with the implementation calling into it.

## Rejected alternatives

- **Rows as an array of records.** Rejected: it lands next to the frame surface instead
  of joining it, and every consumer would immediately convert.
- **A connection handle first** (`db = sqlite("app.db")`, then `db.query(…)`). Deferred,
  not rejected — it needs a new opaque `Value` variant, and the free-function form is a
  complete capability without one. Staged like ADR 0033, so the first stage ships.
- **`db` implying `dataframes`.** Measured at 63 MB on the appliance profile, for a
  backend it does not use. The seam exists precisely so this is not necessary.
- **String-formatted SQL with an escaping helper.** Rejected on the same grounds as a
  shell form in ADR 0037: an escaping helper is a burden the caller must remember, and
  the one place they forget is the one that matters.
- **A general `sql(url, …)` covering every engine behind one verb.** Rejected for now:
  SQLite is a file and Postgres is a socket, so they carry *different capability
  categories* (`fs-read` vs `net`). One verb would have to claim both, and a label that
  overstates is the thing D3 exists to avoid.

## Consequences

- Helix can read from the place data lives, and the result is a first-class frame.
- The appliance profile grows 1.9 MB **only when asked for**.
- The project takes on `rusqlite` + bundled SQLite C. SQLite is about as stable a
  dependency as exists, but it is C in the build, and `cargo audit` now covers it.
- `tests/compat/` gains a shape it cannot express: a program whose output depends on a
  *database file*. Like the argv gap in ADR 0037, the baseline would need a fixture
  column. Not needed yet — no tracked program queries a database.

## Open questions / staging

- **Stage 2 — writes.** `sqlite_execute(path, sql, params) -> Int` (rows affected), with
  `fs-write`. Transactions are the real design question: a Helix expression has no natural
  scope to hang a transaction on, and `try` is the only existing failure boundary.
- **Stage 3 — PostgreSQL.** Effect `net`, so it is capability-gated differently from
  SQLite and needs ADR 0031's hardening arguments (TLS, timeouts, connection limits). The
  sync-vs-async question is the fork: `tokio-postgres` pulls a runtime the appliance
  profile has spent effort avoiding, and `postgres` (the sync wrapper) still pulls tokio.
  That measurement should happen before the API is designed, not after.
- **Type fidelity.** SQLite has no DATE type and Helix has no date type either, so dates
  arrive as text today. That is honest but not useful, and it is the same gap ADR 0030
  (time) is circling.
- **Streaming.** A query that returns more rows than memory is materialised whole here.
  `read_csv` has the same property, so this is not a new problem, but it is a real one.
