# ADR 0044 — PostgreSQL, spoken directly

- **Status:** **Accepted 2026-08-31; implemented.** `postgres_query(url, sql, params?)`
  and `postgres_open(url)` behind `--features postgres`, verified against a live
  **PostgreSQL 19 Beta 3** server: typed columns, `NULL` → `missing`, parameters bound as
  values, read-only enforced by the server, SCRAM-SHA-256 with the server's own signature
  verified, **TLS 1.3 with the certificate chain and hostname checked**, and the capability
  sandbox refusing it without `net`.
- **Date:** 2026-08-31
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0038](0038-database-access.md) (whose four decisions this reuses
  wholesale), [ADR 0021](0021-capability-sandbox.md) (the label must be the truth),
  [ADR 0024](0024-total-runtime.md) (no host aborts — and every byte here is off a socket),
  [ADR 0001](0001-missing-propagation.md) (`NULL` is `missing`),
  [ADR 0032](0032-appliance-profile.md) (gate the body, not the name).

## Context

ADR 0038 gave Helix a database surface and chose SQLite for it. That was the right first
database — bundled, no server, no network — and it settled the four decisions that matter:
a query returns a **DataFrame**, parameters are **values**, the connection is **read-only**,
and the body is **feature-gated**.

It did not settle which databases. PostgreSQL is where the data actually is.

## Decision

**Reuse every ADR 0038 decision, and speak the wire protocol directly.**

### D1 — The same four decisions, unchanged

`postgres_query(url, sql, params?)` returns a `Df` through `backend::build_frame`, binds
parameters as values, runs read-only, and gates its body. Someone who knows `sqlite_query`
knows this; the differences are the ones the two databases genuinely have — a URL instead
of a path, `$1` instead of `?`.

### D2 — Hand-rolled, because the alternatives cost more than the protocol does

Protocol v3 has been frozen since 2003. PostgreSQL 18 introduced 3.2 (256-bit cancel keys)
and 19 carries it, but backward-compatibly — `libpq` itself still requests 3.0 by default —
so a 3.0 client reaches every server from 7.4 to 19. **There is nothing to negotiate**,
which is what makes hand-rolling reasonable rather than reckless.

Against that, every alternative costs a stack:

| option | cost |
|---|---|
| `libpq` | a C library to install — ends the binary's "no system dependency" property, the same property that made SQLite a bundled C build |
| `tokio-postgres` / `postgres` | an async runtime, in a synchronous language |
| **hand-rolled** | **no new dependencies at all** |

The third row is not a boast, it is arithmetic. SCRAM-SHA-256 needs SHA-256, HMAC, base64
and a CSPRNG; TLS needs rustls. `sha2`, `hmac` and `base64` are already CORE dependencies
(the crypto builtins), `OsRng` arrives with `aes-gcm`, and rustls is already linked through
`ureq`, which ships in the default features. PBKDF2 is a loop over HMAC and is written out
rather than imported for eleven lines.

### D3 — Read-only is enforced by the SERVER, because a socket has no read-only mode

This is the one ADR 0038 decision that could not be carried over as written. `sqlite_query`
earns its `fs-read` label by opening the file `SQLITE_OPEN_READ_ONLY`; there is no
equivalent flag for a TCP connection.

So the guarantee comes from the far end: every query runs inside
`begin transaction read only`, and the server refuses `INSERT`, `UPDATE`, `DELETE` and DDL
itself. Asserted, not assumed — the test demands `SQLSTATE 25006` back.

**The capability label is `net`, not `fs-read`.** The read-only property is real, but the
authority being spent is the network, and ADR 0021's audit log has to say what was actually
exercised.

### D4 — SCRAM-SHA-256 only, and the server is verified too

`password_encryption` has defaulted to `scram-sha-256` since PostgreSQL 14. MD5 and
cleartext are **refused by name** rather than implemented: offering them means a client that
silently downgrades when a server asks it to, which is the entire problem with having them
available.

The password never crosses the wire, and **the server's final signature is verified**.
Skipping that check is easy and common; it leaves the exchange authenticating the client to
the server but not the server to the client, which is precisely the half that matters when
someone is in the middle.

### D6 — TLS is on by default, and the SERVER cannot turn it off

`libpq` defaults to `sslmode=prefer`, and so does Go's `pgx`: the client asks for TLS, and
**if the server answers `N`, the session continues in plaintext**. An attacker on the path
does not need to break TLS — they answer one byte and read the password exchange. `require`
is the next rung and barely better: it encrypts but verifies no certificate, so anyone who
can answer on port 5432 can present any certificate and be believed. Six modes exist and
four of them are traps with names.

Helix takes two. **`verify-full`** is the default and what writing nothing gets you: TLS
mandatory, chain to a trusted root, certificate matched against the host. **`disable`** is
plaintext, spelled out by the person who wants it. The property that matters is that the
*server* can never cause the downgrade — there is no mode in which `N` is an acceptable
answer, so the choice is made once, in the caller's own URL, and nothing on the network can
revise it. This is the same principle as refusing MD5 below: a client that downgrades on
request is worse than one that says no.

Everything a rejected mode would have bought is still reachable. A private or provider CA
is a **file** (`sslrootcert=`), which replaces the anchor set rather than switching
checking off. The default anchors are the Mozilla root set (`webpki-roots`) — the same ones
the HTTP client already trusts, so the binary has one trust story and needs no populated
OS certificate store.

An unknown parameter or an unknown `sslmode` value is an **error**, never ignored:
`sslmode=requrie` silently meaning "the default" is the benign twin of it silently meaning
`prefer`, and the capability sandbox has already failed open once on exactly that shape of
typo. Each refused mode's message names what the mode would have cost, because "not
supported" is not something a reader can act on.

Cost, measured against the live server (min of 7, load 1.41): **+1.4 ms per connection**
and nothing per query — 4.73 → 6.23 ms for five queries on one connection, 20.8 → 28.0 ms
for five queries on five. TLS is a per-*connection* cost, which is precisely why D7's
connection value matters more once it is on.

Zero new crates: `rustls`, its `ring` provider and `webpki-roots` are already compiled in
for HTTPS. They become direct dependencies of the `postgres` feature so that
`--no-default-features --features postgres` is self-sufficient rather than silently
depending on `http`.

### D7 — A connection is a value, and its lifetime is the value's

Every `postgres_query` opened a TCP connection and completed a SCRAM exchange: 4.7 ms, the
same for `select 1` as for a whole table — the handshake IS the query time. Removing the
read-only transaction's two round trips moved it by 0.01 ms, which is the proof that the
round trips were never the cost.

`postgres_open(url)` returns a connection that answers `c.query(sql, params?)`. Five
queries: **20.7 ms through five connections, 6.0 ms through one**, with queries 2–5 costing
~0.33 ms each instead of 4.7.

**There is no `close` to forget.** Helix values are reference-counted rather than
collected, so the socket shuts when the last handle goes — deterministically, the same
lifetime rule `Lock` relies on. That removes the failure every connection pool eventually
grows a leak detector for. A scope-callback form (`postgres_with(url, fn)`) was built first
and withdrawn: the walker makes `Value::Function` and the VM makes `Value::Closure` while
`call_builtin` is shared by both, which is why this codebase has higher-order *methods* and
no higher-order *builtins*.

### D8 — A `Connection` owns its method names

`Connection` was in no `registry::type_method_tables()` entry, so `type_owns_method`
answered false for every name — including `query`. That is the predicate ADR 0045's
fallback uses to decide when NOT to retry, so a user's own `fn query(c, sql)` silently
answered `c.query(...)` instead of the database. With matching arities there is no error,
just a program that never reaches the server.

It was the only type in that position, because it was the only `Value` variant added
without a table. The table exists in every build even though only `--features postgres` can
construct a `Connection` — ADR 0032's gate-the-body rule — which also makes
`helix doc Connection` answer everywhere.

### D5 — Unknown column types read as text rather than failing

`int2/int4/int8` → Int, `float4/float8/numeric` → Float, `bool` → Bool, everything else →
the text the server printed. So `uuid`, `jsonb`, timestamps, ranges, extension types and
domains all read on day one.

This is ADR 0033 Stage 2's rule for foreign parquet dtypes, applied for the same reason:
refusing a column because the reader has no opinion about it is worse than handing back what
the server said. `numeric` → Float is the one lossy mapping, taken deliberately — money is
the motivating case and arithmetic is what people do with it — and the column's type is
visible in `describe`, which is where a reader can see the trade.

## Honest costs

- **A nullable `boolean` reads as text.** `ColData` has `Bool(Vec<bool>)` with no nullable
  form, so a boolean column containing NULL cannot become a Bool column without inventing a
  value for the null. It reads as `"t"`/`"f"`/`missing` instead: lossless, and visibly a
  string. `ColData::BoolOpt` is the real fix and belongs with both backends rather than
  being smuggled in here.
- **No channel binding.** `SCRAM-SHA-256-PLUS` binds the authentication exchange to the
  TLS session, so a proxy holding a mis-issued certificate still cannot replay it. It is
  not implemented. The gap it closes is narrower here than in `libpq`, because there is no
  `require` mode to be sitting in — every TLS session is chain- and hostname-verified — but
  it is a real gap and it is the next thing this file should grow.
- **One statement per call.** No multi-statement batch and no cursor: a query is sent,
  executed, and fully read. A result larger than memory has no streaming form yet.
- **Type inference is the server's.** Parameters are sent with unspecified OIDs so the
  server infers each from its use, which is what `libpq` does. A parameter in a position
  the server cannot infer (`select $1`) needs a cast, exactly as it does from psql.

## Rejected alternatives

- **A driver crate.** Rejected on dependency cost: an async runtime for a synchronous
  language, to speak a protocol that has not changed since 2003.
- **Binary result format.** Rejected: a decoder per OID and a new failure mode per decoder,
  for a saving invisible next to the network round trip — and it would have made the
  unknown-type case a refusal instead of text.
- **Supporting MD5 auth.** Rejected: a client that downgrades on request is worse than one
  that says no. `sslmode=prefer` is the same sentence about a different layer, and is
  refused for the same reason.
- **`sslmode=require` and `verify-ca`.** Rejected as named traps: the first encrypts
  without checking who answered, the second checks the chain but not the hostname, so a
  valid certificate for another host passes. Both are refused with a message saying so —
  the reader who reaches for them is trying to make TLS work, and the answer they need is
  `sslrootcert=`, not a mode that stops checking.
- **The OS certificate store.** Rejected in favour of the Mozilla set the HTTP client
  already uses: one trust story per binary, and no failure mode where a scratch container
  with no `/etc/ssl` trusts nothing at all.
- **`fs-read` for symmetry with SQLite.** Rejected: it would be false. The label has to
  name the authority actually spent.

## Addendum 2026-09-04 — writes

The capability this ADR deferred writes behind exists: [ADR 0047](0047-database-writes.md).
D3 stands unchanged for the read verbs — a query session is read-only from its first byte —
and a session that can write is a *different* session, opened by `postgres_execute` or
`postgres_open(url, "write")`, spending `db-write` as well as `net`.

