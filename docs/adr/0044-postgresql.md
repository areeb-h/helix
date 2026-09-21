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
domains all read on day one. (Text is the format every column CAN be read in, and the one a
statement's first run uses; since 2026-09-20 five fixed-width types cross in binary on later
runs — the same values, see that addendum.)

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
- ~~**One statement per call.**~~ Several statements share a round trip since 2026-09-21 (the
  addendum of that date). Still no cursor: a query is sent, executed, and fully read, and a
  result larger than memory has no streaming form yet.
- **Type inference is the server's.** Parameters are sent with unspecified OIDs so the
  server infers each from its use, which is what `libpq` does. A parameter in a position
  the server cannot infer (`select $1`) needs a cast, exactly as it does from psql.

## Rejected alternatives

- **A driver crate.** Rejected on dependency cost: an async runtime for a synchronous
  language, to speak a protocol that has not changed since 2003.
- **Binary result format — for everything.** Rejected: a decoder per OID and a new failure
  mode per decoder, and it would have made the unknown-type case a refusal instead of text.
  REVISITED 2026-09-20 for exactly five types, once a measurement said what it buys: the
  saving is not invisible on a result of any size, because for `float8` it is the SERVER's
  printing that binary removes. Text stays the universal format; see that addendum.
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

## Addendum 2026-09-19 — a statement is prepared once per connection

Every query used to be Parsed from scratch as the unnamed statement, so the server parsed,
analysed and rewrote the same text on every call. The web field build measured what that
costs against pgx and GORM on one PostgreSQL 17: Helix's raw connection matched pgx with its
statement cache OFF, and that cache was worth 23–57 µs of a ~150 µs round trip — the whole
gap to GORM on a small query (§1.61). A library cannot close it itself: SQL-level `EXECUTE
p($1)` refuses a bound parameter, so the only client-side cache is one that splices values
into text, which is the one thing a query layer must never do.

A connection opened with `postgres_open` now Parses each distinct text ONCE under a name and
thereafter only Binds and Executes it (`Prepared` in `src/pg/mod.rs`): at most 256 statements,
the least recently used one closed on the server in the same round trip that parses its
replacement. Nothing a caller can see changes. Parameter types were already inferred from the
text alone — Parse never saw the values — and the three ways a name goes stale are handled
where they surface: the statement gone (`26000`: `DEALLOCATE`, `DISCARD ALL`, a pooler's
other backend) or its result type changed under it (`0A000`: `ALTER TABLE` beneath a prepared
`select *`) is prepared again, once; a name a user's own `PREPARE` already took (`42P05`)
falls back to the unnamed statement, which always works. A text that does not parse is never
remembered; one that parsed and was then refused at Bind (a parameter of the wrong shape) is,
because the statement exists. There is no knob: behind a transaction-mode pooler the cache
stays correct and merely re-prepares. The one-shot verbs (`postgres_query(url, …)`,
`postgres_execute(url, …)`) keep the unnamed statement — a connection that lives for one
query has nothing to reuse.

With it, one exchange became ONE write. Parse, Bind, Describe, Execute and Sync were five
flushed writes — five syscalls, five segments with `TCP_NODELAY` on, five records under TLS —
and are framed into one buffer and sent together, which the extended protocol is built for.

And the connection READS THROUGH A BUFFER. `read_msg` takes a message as its 5-byte header
and then its body, and on a bare socket that was two `read` calls for every `DataRow`: a
1 000-row result was ~2 000 syscalls, and that — not text parsing, which the field build
reasonably inferred — was most of what the read cost against pgx (3.7x). It also made a
result's time depend on the server's pacing, each tiny read either finding its bytes or
blocking for them: preparing statements, which makes the server answer SOONER, read 6%
slower on that row, and the commit script refused it until the cause was found. `Stream`
(`src/pg/tls.rs`) owns a 16 KiB buffer over the plain or TLS socket; a read that size or
larger goes straight through, and nothing is ever read around it. Binary results for
fixed-width columns, the report's other ask, are not needed to close that row and stay
unbuilt until a measurement says the text parse is what remains.

Proven without a server by the fake one in `src/pg` — which learned statement names, `Close`,
the skip-to-Sync an error starts, and how to forget its statements — and against PostgreSQL
17 in a container: every stale-name case answers exactly what the unnamed statement answered.

## Addendum 2026-09-20 — a result is decoded where it arrives, and a statement's columns are remembered

Asked to make the driver faster still, the first thing done was to find out where a round
trip's time goes, from outside the process: its own user and system time against the wall.

| µs per round trip | wall | user | sys |
|---|--:|--:|--:|
| `select 1` | 157 | 7 | 32 |
| find by pk | 192 | 13 | 31 |
| 1 000 rows x 7 mixed columns | 770 | 377 | 160 |
| 1 000 rows x 4 `float8` | 660 | 212 | 110 |
| 10 000 rows x 7 mixed columns | 6 000 | 3 873 | 1 267 |

Two different answers. **A small query has almost nothing left in the client**: 7–13 µs of
its own code in a 150–190 µs round trip, the rest the socket and the server — so nothing
below claims a small-query win, and none was found. **A result of any size was half
client**, 54 ns a cell, and every nanosecond of it allocation:

- each message was a fresh `Vec` — allocated, zeroed, filled from the read buffer, parsed,
  freed — a thousand times for a thousand rows;
- each CELL became a `String` (validated, copied, pushed), and a number was parsed from that
  string in a second pass at the end and the string freed: 7 000 allocations for a 1 000 x 7
  result before a frame existed;
- then the engine interned every text column, hashing each of those strings and — the text
  almost always being in the dictionary already — throwing it away.

**Decoded where it arrives.** `Stream::next_msg` (`src/pg/stream.rs`) lends a message's body
from the buffer it was read into; only one larger than the buffer is gathered, into a second
buffer that is kept. A number is parsed from those bytes straight into the vectors the engine
keeps (`ColData::IntValid`/`FloatValid`: values with their validity alongside, the native
column's own shape). Text is interned AS IT ARRIVES, through the engine's one hash-consing
builder, which moved to the seam for it (`backend::strbuild`, `ColData::StrBuilt`): one
allocation per DISTINCT value where there was one per cell. A query is framed in place in a
buffer the connection keeps (`begin_msg`/`end_msg`), parameters written into it directly.

**What a statement returns is remembered with its name.** The first run asks the server to
describe the result, as every run used to. Later runs neither ask nor wait for that, and —
knowing each column's type BEFORE Bind is sent, which is the only time a format can be asked
for — take five types in binary: `int2`, `int4`, `int8`, `bool`, `float8`. The measurement
that decided it is the `float8` row above against the integer one: the same cells cost 250 µs
more, and that is the server printing each float as the shortest decimal that reads back —
work no client can speed up and binary simply does not do.

It is only ever the same value in another encoding, which is the rule that picked the five.
`float8` text has been exact since PostgreSQL 12, and binary floats are gated on the
server's reported version, because a statement's first run (text) and its later ones
(binary) must never disagree. `float4` and `numeric` STAY text: their text is what Helix's
Float means by them — `1.1`, not the `1.100000023841858` a widened 32-bit float is, and a
decimal parsed once, correctly rounded. Everything else stays text because text is what makes
an unknown type readable at all (D5). NaN is one value in either format. Every decoder is
fixed-width, so the only new failure is "the server sent the wrong number of bytes", which
is an error naming the column.

The remembered columns cannot go stale unnoticed. A named statement's result type is FIXED
on the server: when it replans it compares names, types, typmods and collations, and answers
`0A000` rather than return a different shape — the code that already sends the statement
cache back to Parse. Verified live under a prepared `select *`: add a column, RENAME it,
change its type, drop it, widen an `int2` to `int8` — each answers the new shape on the very
next call, in both formats.

**A connection that can no longer be trusted closes itself.** Found while moving the decoder:
an exchange that did not end at `ReadyForQuery` — the read timed out, the socket dropped, a
cell was not UTF-8 and the read stopped there — left the server's replies unread, and the
connection open. The NEXT statement on it would have read them as its own: not an error, the
wrong rows. Now an error the server reports is read to its end and the connection carries
on; a cell that is not its column's type is remembered while the rest of the result is still
read, then reported; and anything else — a failure with the protocol state unknown — is the
connection's last act. Every later call says `this connection is closed — an earlier statement
on it failed with: …` without touching the socket. A timeout also says what it is
(`the server did not answer within 30 s`) where it said `Resource temporarily unavailable
(os error 11)`. And a `COPY … FROM STDIN` — which makes the server WAIT for rows a Helix
connection has no way to be handed — is refused with `CopyFail` the moment the server asks:
an ordinary error in 0.00 s where it was the whole read timeout and then a dead connection.

Measured live against PostgreSQL 17 in a container (one codegen unit both sides, three
interleaved rounds, min per row; `866aca4` → this), µs per round trip:

| | before | after | |
|---|--:|--:|--:|
| `select 1` | 150.7 | 148.4 | 0.98 |
| find by pk | 188.8 | 187.0 | 0.99 |
| range + order, limit 20 | 207.6 | 194.2 | 0.94 |
| in (3), limit 50 | 255.9 | 240.9 | 0.94 |
| 1 000 rows x 7 mixed | 700.6 | 545.8 | 0.78 |
| 1 000 rows x 4 `float8` | 609.7 | 384.2 | 0.63 |
| 1 000 rows x 4 int | 408.6 | 375.5 | 0.92 |
| 10 000 rows x 7 mixed | 5 612.8 | 3 765.6 | 0.67 |

The same through verified TLS (a throwaway CA, `sslrootcert=`): 0.74, 0.65 and 0.63 on the
three large rows, and the caller-visible script — every type with its NULLs and extremes run
three times, every way a name or a remembered column goes stale, errors mid-result, 300
distinct statements — prints the same 228 lines on both binaries, in the clear and under TLS.

Measured and declined: a 64 KiB read buffer (1.00x everywhere — the server flushes at 8 KiB,
so there is never more than the 16 KiB buffer's worth waiting).

## Addendum 2026-09-20 — an Array is a parameter

D1 made parameters VALUES, and the values were scalars: Int, Float, Bool, String, `missing`.
A list had to be spelled `in ($1, $2, $3)`, which has two costs the statement cache made
visible. Its TEXT changes with the count, so every length of list is a different prepared
statement — parsed again, another of the 256 entries — and the server takes at most 65 535
parameters, so a relation load over 70 000 parents did not get slow, it failed. PostgreSQL's
own answer is `where id = any($1)` with one array parameter, and the field build's ORM was
already using it — by writing the array literal's grammar itself, in Helix, with a fast path
through `to_json` held to a careful slow one. That grammar is the driver's to get right once.

An Array binds as the array literal (`put_array` in `src/pg/statement.rs`): numbers and
booleans bare, `missing` as the bare word `NULL`, and a String ALWAYS quoted, with `\` and
`"` escaped. Always, because a bare element is where every trap lives — `NULL` would be a
null, `a,b` two elements, `{` a nesting, leading spaces would vanish — and a quoted element
is its text and nothing else. It is data for the server's array parser, never SQL, which is
why a String is safe here where splicing one into the statement would not be. A nested
Array is a further dimension, to PostgreSQL's limit of six; the server holds the rectangle
to account. `Bytes` binds as `bytea`, in hex.

Parameters stay TEXT with unspecified types: the server infers `int4[]` from `= any($1)`
against an `int4` column exactly as it infers `int4` from `= $1`. Verified against the
server's own parser, element for element (`target/bench/f92/arrays.helix`): commas, quotes,
backslashes, braces, the text `NULL`, the empty string, non-ASCII, a newline, a `missing`;
`NaN` and infinity in a `float8[]`; two dimensions; an empty array; 70 000 keys in one
parameter; and the same statement with two list lengths is ONE prepared statement.

## Addendum 2026-09-20 — how long a statement may take

TOTALITY put a bound on every wait (the module note: a server that accepts a connection and
then says nothing must not hang a program that cannot be interrupted from inside), and the
bound was one number for everything: a 30 s read timeout. For the handshake that is right. For
a statement it conflated two things. It is a bound on SILENCE, so a result that streams for
ten minutes was always fine, while a statement that COMPUTES for 31 s before its first row —
an aggregate over a large table, which is what a scientific language is for — could not be
run at all. And when it fired, the statement was abandoned rather than ended: the connection
is closed (its reply is still coming; addendum above), and the server carries on producing
an answer nobody will read.

The URL now says (`conninfo::Patience`):

- **nothing** — what it always was: the server may be silent for thirty seconds. The error
  that ends that wait now names the two spellings below.
- **`timeout=N`** — the SERVER ends a statement that runs past N seconds. It is sent as
  `statement_timeout` in the startup packet, the way read-only is: in force from the first
  byte, for no round trip. Running too long is then an ORDINARY error — the server stops the
  work itself, answers `57014`, and the connection carries on; the message adds where the
  limit lives, recognised by the SQLSTATE and the clock rather than by the server's wording,
  which follows `lc_messages`. The client's own wait moves to N + 10 s, so the server's
  verdict always arrives first and the wait is what it should be: a bound on a server that
  has stopped answering altogether.
- **`timeout=0`** — as long as it takes.

`connect_timeout=N` (10) bounds the TCP connection; the handshake's own 30 s is not the URL's
to move, because a server that goes quiet mid-handshake is broken, not busy.

Every connection also asks the kernel to notice a peer that has gone (`keep_alive` in
`src/pg/connect.rs`: a probe after a minute's quiet, then every ten seconds, six unanswered is
dead — `libc`, already a dependency, Unix only). With `timeout=0` that is what keeps "as long
as it takes" from meaning "forever" when the host at the other end lost power; on a
long-lived connection it turns the next statement's long wait into a prompt error.

Rejected: making a limit the DEFAULT. `statement_timeout` bounds a statement's whole life,
streaming included, so a default would end large reads that work today. The default's meaning
is unchanged; only its error message grew.

Rejected: the client giving up and sending `CancelRequest`. That is what a client must do
when only IT knows the limit; the server knowing it is strictly better — no second connection,
no race between the cancel and the reply, and the connection survives.

## Addendum 2026-09-21 — several statements, one round trip

The 2026-09-20 profile said where a small query's time is: 7–13 µs of this client's code
inside a 150–190 µs round trip. Nothing done to a single statement makes a page of five
queries faster. Sending the five together does.

```helix
page = c.query([
  {sql: "select * from people where id = $1", params: [id]},
  {sql: "select * from posts where author_id = $1 order by id desc limit 20", params: [id]},
  "select count(*) as n from people"])
person = page[0]
posts = page[1]
```

**No new verb.** `query` and `execute` take an Array of statements and answer an Array, in
order — frames from `query`, `{affected, rows}` from `execute`. A statement is a SQL String, or
a record with `sql` and (when it has parameters) `params`, which is the shape a query builder
renders to anyway; other fields are not looked at. The capability gate keys on the verb's
name, so `execute` handed a flight spends `db-write` exactly as it does handed one statement.

**One Sync, so one transaction.** Each statement is framed as it would be alone — Parse only
if this connection has not prepared its text, Describe only if what it returns is not yet
known, binary where that is known to be the same value — and ONE Sync ends them all. The
server commits at Sync; a statement that fails makes it skip every later one and roll back
every earlier one. So a flight is all or nothing without anyone saying `begin`, and what comes
back is every answer or one error naming its statement (`statement 2 of 3: …`). Inside a
transaction's value it is part of that transaction.

**The cache is planned before anything is framed.** A text met twice in a flight is parsed
once. Every statement the flight BINDS is claimed before any new one chooses whom to displace
— including one that comes later in the flight than the newcomer. Choosing a victim no longer
forgets it (`Prepared::reserve`/`forget`): it leaves the cache when the `Close` naming it has
been read by the server, so a flight that fails halfway remembers exactly what the server has
— the statements parsed before the failure, not the ones after it.

**It cannot deadlock.** Written first and read afterwards — how every single statement goes,
safely, because the server has nothing to say until it has read the statement — a flight whose
request is larger than the socket buffers, against an answer larger than them, stops both
ends for ever: the server blocked sending rows nobody is reading, the client blocked writing
statements nobody is reading. `Stream::send_draining` sends on a socket that does not block:
what the kernel will not take yet waits, what has arrived is set ASIDE (never parsed there),
and when neither direction moves `poll` says when one can. Under TLS the same loop drives
rustls's record layer directly — plaintext in, records out, records in, plaintext set aside.
Every later read serves what was set aside first. A flight of ONE is simply that statement.

**Two things the live server taught that the fake one had wrong.**

- After `DEALLOCATE ALL` (or `DISCARD ALL`, or a pooler's other backend) EVERY name a flight
  binds is stale, and one that forgot only the statement the server happened to refuse was
  refused for the next. A stale name now makes the flight forget every cached statement it
  binds — and `Close` them at the head of the next attempt, so that one which was still there
  (`0A000` spares the others) is not left behind — and go again, once. The rollback makes that
  safe: nothing of the first attempt happened. (The single-statement path now closes its
  stale statement the same way; it used to leave a `0A000` statement on the server.)
- `COPY … FROM STDIN` followed by other statements does NOT make the server skip to the Sync.
  It reads the next statement as COPY data, has then read one byte of a message it will not
  finish, and ENDS THE CONNECTION (`protocol synchronization was lost`). Once the flight is on
  the wire nothing can be done, so a flight containing a `COPY` — any form; telling `FROM
  STDIN` from `FROM 'file'` needs the server's parser — is refused before anything is sent.
  The fake server was rewritten to do what the real one does.

Measured live against PostgreSQL 17 (µs per group, one connection, min of 5 trials):

| | one by one | together | |
|---|--:|--:|--:|
| 5 x find by pk | 998.3 | 281.2 | 3.55x |
| a page: find + 20 rows + count | 682.3 | 346.8 | 1.97x |
| 1 statement (a flight of one) | 199.7 | 199.9 | 1.00x |

and 120 statements carrying 7 MB out against 14 MB back — far past any socket buffer — answer
correctly in 0.06 s in the clear and 0.07 s under TLS. The behaviour script prints the same 16
lines under the walker, the VM and the JIT, in the clear and under TLS.

**The gate has TLS data-path tests for the first time** (`src/pg/tls_wire_tests.rs`). No
server is needed, only a peer that speaks TLS: rustls's own server side behind the one-byte `S`
a PostgreSQL server answers the SSLRequest with. Its certificate is made at test time — a
self-signed Ed25519 certificate for `localhost`, the DER written by hand and signed with
`ed25519-dalek` — so no key is checked in, and the client trusts it the only way it trusts
anything: as an `sslrootcert` file through `tls::negotiate`, chain and name verified. Both
deadlock tests were shown to have teeth: with the blocking send they wedge until their timeouts.
