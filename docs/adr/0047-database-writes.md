# ADR 0047 — Database writes are a different session, and spend their own grant

- **Status:** **Accepted 2026-09-04; implemented.** `postgres_execute(url, sql, params?)`,
  `postgres_open(url, "write")` with `execute(sql, params?)` on the connection, the
  `db-write` effect, `HELIX_ALLOW_DB`, and `[capabilities] db = "write"`.
- **Date:** 2026-09-04
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0044](0044-postgresql.md) (the read verbs: a session the SERVER holds
  read-only from its first byte, labelled `net`; it deferred writes "behind an explicit
  capability" — this is that capability), [ADR 0021](0021-capability-sandbox.md) (the label
  must be the truth), [ADR 0037](0037-process-and-env.md) (a category that cannot be
  granted is a wall, not a sandbox), [ADR 0038](0038-database-access.md) (parameters as
  values; injection unrepresentable).

## Context

ADR 0044 made every PostgreSQL session read-only in the startup packet — no window, no round
trip, enforced at the far end — and labelled the verbs `net`, because that is the authority
they spend. It deliberately left writes out. A field build then wrote an ORM comparison in
Helix and stopped at `Create`: a language that can read a database but not change one is
not one you can build a model layer in.

Read-only being a property of the *session* is the constraint that shapes the answer. A
write cannot be "a query that is allowed to write": the session it would run in refuses it
before the client's intent is known. A write needs a session opened differently — and
opening such a session is an authority of its own, which `HELIX_ALLOW_NET=on` must not grant
by accident, or every read-only program becomes a writer the moment its network is allowed.

## Decision

**D1 — Two spellings, one verb.** `postgres_execute(url, sql, params?)` runs one statement
that may write. `postgres_open(url, "write")` opens a connection that can, and its
`execute(sql, params?)` is the same verb on the reused socket. `query` still works on such a
connection; `execute` on a read-only connection is refused *before a byte is sent*, with the
spelling that opens a writable one (the server would refuse too — SQLSTATE 25006 — a round
trip later and without saying what to do).

**D2 — The answer is `{affected, rows}`, always.** `affected` is the count from the server's
completion tag (`INSERT 0 3` → 3; DDL → 0). `rows` is a frame of what the statement
returned: empty unless it has a `RETURNING` clause, which is how an inserted id comes back
in the same round trip rather than through a second, racy query. The shape is fixed, so the
checker knows it, and `let {affected, rows} = postgres_execute(…) in …` type-checks.

**D3 — The session is the authority.** The startup packet omits
`default_transaction_read_only` only for a writable session, and the grant is checked where
that session is opened: at `postgres_execute`, and at `postgres_open(url, "write")` — in
every build, before any network. `execute` is gated by name as well, the way `write_to` is.

**D4 — `db-write` is its own effect, and needs `net` too.** `Effect::DbWrite`, label
`db-write`, granted by `HELIX_ALLOW_DB=write` (or `all`) and by `db = "write"` in a
manifest's `[capabilities]`. `allows(DbWrite)` is `net && db_write`: the database is reached
over the network, and a write is more than a network access. There is no `read` value —
reads are the `net` grant, as ADR 0044 decided. A value that does not parse is refused at
startup, like every other grant.

**D5 — One statement is one transaction.** It commits when it completes; a failed one
changed nothing. A transaction spanning statements is not offered; it is the open item.

**D6 — Verified without a server, and with one.** A fake server in `src/pg` speaks enough of
the protocol to prove what the client sends — the read-only startup parameter present for a
query and absent for a write — and to answer rows and a completion tag. The gate now builds
`--features postgres`, so those tests run in every gate; they were the one feature whose
tests ran nowhere. Live verification against a real PostgreSQL is the field build's.

## Consequences

- A model layer can be written in Helix: create, update, delete, and read the id back.
- `HELIX_ALLOW_NET=on` still keeps a program read-only against every database it can reach.
  The audit mode reports `db-write` by name, so the footprint of a program that writes is
  visible before anything is enforced.
- The gate builds rustls once more than it did. It costs a compile, not a dependency the
  shipped default binary carries.

## Alternatives considered

- **Let `postgres_query` write when granted.** Rejected: the read verb's label would stop
  being the truth (ADR 0021), and a program could not tell which of its queries could change
  the database.
- **Cover writes with `net`.** Rejected: granting the network would silently grant writes.
  A finer grant costs one variable and makes the coarse one keep its promise.
- **Return the count only.** Rejected: it loses `RETURNING`, and an inserted id would need a
  second query that can observe another writer's row.
- **A transaction API now.** Deferred (D5). It needs a design for a connection that holds
  state across calls and what happens when the value is dropped mid-transaction.
