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
changed nothing. ~~A transaction spanning statements is not offered; it is the open item.~~
**Closed 2026-09-20: a transaction is a value** — see the addendum.

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
  state across calls and what happens when the value is dropped mid-transaction. (Both
  answered on 2026-09-20, and the second answer IS the design: it rolls back.)

## Addendum 2026-09-20 — a transaction is a value, and its lifetime is the value's

```helix
fn transfer(c, n) = do {
  tx = c.begin()
  _ = tx.execute("update acct set bal = bal - $1 where id = 1", [n])
  _ = tx.execute("update acct set bal = bal + $1 where id = 2", [n])
  tx.commit()
}
```

`c.begin(isolation?)` answers a CONNECTION VALUE that speaks for the transaction. It takes
`query` and `execute` like the connection it came from — `type_of(tx)` is `"Connection"`, so a
model layer written against a connection takes it unchanged (`People.on(tx)`) — and ends with
`tx.commit()` or `tx.rollback()`.

**One that is dropped without committing rolls back.** That is the whole design, and it is
ADR 0044 D7's rule applied once more: Helix values are reference-counted, so "dropped" is a
moment, not an eventuality. An error raised between `begin` and `commit` unwinds past `tx`,
and the `ROLLBACK` has been sent by the time a `try` around it answers — on the walker, the
VM and the JIT alike, verified live on all three. So the function above commits when both
updates succeeded and undoes both when either raised, with nothing for its author to
remember: there is no `close` to forget, and now no `rollback` either.

The field build asked for `conn.transaction(tx => do { … })`, the shape every other language
uses because every other language has to: a callback is how you get a guaranteed cleanup
without deterministic destruction. Helix has deterministic destruction, so the callback buys
nothing — and it costs what ended `postgres_with` (ADR 0044 D7): a builtin cannot call a
closure the same way on the walker and the VM. A library that wants the callback spelling
writes it in three lines over this one: `fn transaction(c, f) = do { tx = c.begin(); r =
f(tx); _ = tx.commit(); r }`.

**While a transaction is open, its value is the only way in.** The session is one, so a
statement sent through the connection's own value would land INSIDE the transaction,
silently. It is refused instead, before anything is sent, naming the transaction's value. A
value whose transaction has ended refuses everything the same way. There is no nesting —
`begin` on a transaction's value is refused by name; savepoints are not offered.

**A failed transaction cannot commit.** An error inside a transaction ends it on the server:
every later statement is answered `25P02` until it rolls back. And the server answers
`COMMIT` on such a transaction with the tag `ROLLBACK` and NO error — which any caller would
take for success. The session's standing arrives with every `ReadyForQuery` (`I` idle, `T` in
a transaction, `E` failed) and is kept; `commit()` on a failed transaction sends `ROLLBACK`
by name and raises `the transaction was rolled back, not committed`.

**The isolation level is one of three sentences**, never text a caller supplied:
`begin()`, `begin("read committed")`, `begin("repeatable read")`, `begin("serializable")`.
On a READ-ONLY connection `begin("repeatable read")` is how several queries see one snapshot,
which is what a report made of five queries needs and had no way to ask for.

**`begin()` refuses a session already in a transaction begun in SQL** (`execute("begin")` still
works, as it always did, and is the caller's to end): a value that rolled such a transaction
back when dropped would be undoing work it was never given.

**One honest cost.** Inside a transaction, ANY error ends it — including the two the
statement cache normally absorbs (`26000`, `0A000`: a prepared statement made stale by a
concurrent `ALTER TABLE` or `DISCARD ALL`) — except in the transaction's FIRST exchange, which
carries its BEGIN and is prepared again (addendum 2026-09-26). Outside a transaction those are re-prepared
invisibly; inside one the re-prepare could only be answered `25P02`, so the original error is
reported instead, saying what happened and what to do: roll back and run it again. Every
driver that prepares statements has this property; the ones that hide it do so with a
savepoint per statement, a round trip each.

No new capability: `begin` spends nothing a connection did not already hold. What a
transaction can DO is decided where the session was opened — `execute` through a transaction's
value is `db-write` exactly as it is through the connection.

## Addendum 2026-09-26 — the BEGIN rides with the transaction's first exchange

`tx = c.begin()` sent `BEGIN` in a round trip of its own, before the caller had said anything
the transaction was for. It sends nothing now: the transaction's value OWES its BEGIN
(`Shared::begin_owed`), and the transaction's first exchange — a statement, a flight or a
cursor — carries it at its head, in the same round trip.

- **The same transaction.** PostgreSQL takes a transaction's snapshot at its first statement,
  not at `BEGIN` — READ COMMITTED takes one per statement anyway — so a BEGIN sent with the
  first statement starts the transaction a caller would have had. Verified live against the
  parent commit: a `repeatable read` transaction whose second connection commits a row between
  `begin()` and its first statement, and another after it, sees the same counts on both.
- **A round trip sooner.** `begin`, one update and `commit` took 545 us and take 390
  (paired median 0.71): three round trips became two. A transaction of N statements costs
  N + 1 round trips where it cost N + 2.
- **A transaction that sends nothing never reaches the server.** Committed, rolled back or
  dropped, it has nothing to end; each still ends its value, and the connection is its own.
- **A stale name in the first exchange is prepared again** — the one place the "honest cost"
  above gives way. The BEGIN went with it, so the transaction holds nothing of the caller's yet:
  it is rolled back and the exchange goes again, once, BEGIN and all. Behind a pooler that moved
  the session to another backend between transactions, a transaction's first statement used to
  fail it; now it just runs. A later exchange still reports the error with what to do.
- **The BEGIN is owed until an exchange that carried it leaves the session inside a
  transaction.** A BEGIN that failed, or a flight that could not be framed and was never sent,
  leaves it owed; an error in the caller's first statement has begun (and failed) the
  transaction, as it always did, and `commit()` rolls back and says so. An error in a first
  FLIGHT is counted among the caller's statements (`statement 2 of 2`), not the BEGIN.
