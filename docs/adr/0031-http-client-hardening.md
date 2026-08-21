# ADR 0031 — What does an HTTP client owe a program that trusts it?

- **Status:** **Proposed 2026-08-20** — recommendation argued below; awaiting owner
  acceptance. Nothing implemented.
- **Date:** 2026-08-20
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0021 — Capabilities](0021-capabilities.md) (`Net` is a declared
  effect, and everything here happens inside it),
  [ADR 0024 — Total runtime](0024-total-runtime.md) (no host panic, no abort, from any
  input — including a hostile server's),
  [ADR 0003 — Collection API](0003-collection-api.md) (one verb per concept),
  [ADR 0020 — Reproducibility](0020-dict-ordering-and-reproducibility.md) (a redirect
  chain must not make one program two answers).

## Context

v0.3.0 made the HTTP client usable: connections are pooled, headers and JSON bodies have
a literal syntax, cookies parse in both directions, and percent-encoding exists. It is
now good enough that people will point it at real services, which is exactly when the
remaining gaps stop being ergonomics and become a security surface.

Four gaps were named and deliberately left, and they are not four independent features.
Three of them are the same question — **what may cross a boundary?** — asked about
credentials, about cookies, and about methods:

1. **Redirects** do not preserve the method, and nothing is dropped when a redirect
   leaves the origin.
2. **Cookies** parse but nothing decides whether a cookie may be *stored* or *sent*.
3. **Timeouts** are agent-wide, so one slow endpoint cannot be bounded separately.
4. **Header lookup** is case-sensitive over a Dict, so `get("Content-Type")` returns
   `missing` while `get("content-type")` works.

The fourth is ergonomics. The first three are where HTTP clients get CVEs.

### What actually goes wrong, in other people's implementations

This is the part worth being specific about, because the failure modes are documented
and repeated:

- **Credential leakage across a redirect.** A client that replays `Authorization`,
  `Cookie`, or `Proxy-Authorization` to whatever host a `Location` names hands the
  caller's bearer token to that host. This is the single most common HTTP-client CVE
  class; curl, requests, and several language stdlibs have each shipped and then fixed
  a version of it. The rule every one of them converged on: **strip credential headers
  when the redirect changes origin** (scheme, host, or port).
- **SSRF via redirect.** A URL the program vetted can redirect to one it did not:
  `http://169.254.169.254/` (cloud instance metadata), `http://127.0.0.1:*`, or a
  `file:`/`gopher:` scheme. The vetting happened on the first URL and nothing re-vetted
  the second.
- **Protocol downgrade.** An `https://` request that follows a redirect to `http://`
  moves the rest of the exchange, and anything replayed with it, onto the wire in clear.
- **Cookie scope confusion.** Without the Public Suffix List, a page on `evil.co.uk` can
  set a cookie for `.co.uk` — a *supercookie* readable by every site under that suffix.
  The domain-matching rules in RFC 6265 §5.1.3 are necessary but not sufficient; the
  PSL is what makes them safe.
- **`Secure` ignored.** A cookie marked `Secure` that is sent over plain HTTP is the
  attack the flag exists to prevent.
- **Unbounded input.** A redirect loop, a response with no end, or a `Set-Cookie`
  avalanche each turn a request into a hang or an OOM. ADR 0024 says the runtime is
  total: a hostile *server* is input like any other.

### What this project's own doctrine already decides

Two of these answer themselves from existing ADRs, which is worth noticing before
inventing policy:

- **ADR 0024 (total runtime).** Every limit below — redirect count, body size, header
  count, cookie count — exists because "the server said so" is not a reason to abort or
  exhaust memory. These are not tuning knobs; they are the same commitment applied to
  the network.
- **ADR 0021 (capabilities).** `Net` is already a declared effect. Nothing here needs a
  new capability, but the *reach* of that capability is exactly what redirect policy
  decides, and that argues for the safe behaviour being the default rather than an
  opt-in.

## Decision

Four changes, in the order they should be built, with the security-relevant behaviour as
the default in every case and the unsafe form available only by explicit request.

### 1. Redirect policy — the boundary rules

Follow redirects by default, to a **limit of 10**, and on each hop:

- **Preserve the method**, except the one historical exception: `301`/`302`/`303` after
  a `POST` becomes a `GET`, which is what every client does and what the web depends on.
  RFC 10008 §3 is explicit that this exception **does not apply to `QUERY`** — a QUERY
  redirect stays a QUERY, with its body. `307`/`308` preserve method and body for
  everything, as RFC 9110 requires.
- **Strip credential headers when the origin changes.** `Authorization`, `Cookie`, and
  `Proxy-Authorization` are dropped when scheme, host, or port differ from the previous
  hop. This is not configurable: a client that offers "send my token anywhere" as a flag
  will have that flag set by someone who did not read this.
- **Refuse a downgrade.** `https:` → `http:` is an error, not a silent follow.
- **Refuse a scheme change** to anything that is not `http`/`https`.
- **Re-resolve, do not trust.** Each hop's URL is subject to the same checks as the
  first.

The redirect chain is returned as data — `{status, body, headers, redirects: [url, …]}` —
because a program that cares where it ended up should not have to guess, and because a
chain that was followed silently is a chain nobody audited.

**Rejected: following redirects to private address ranges by default.** It is tempting
to block `127.0.0.0/8`, `169.254.0.0/16`, `10/8` and friends outright, and for a
server-side client that is right. But Helix programs legitimately talk to
`http://127.0.0.1:11434` (a local model) and `http://localhost:5432`-shaped services all
the time — the field libraries do it in the examples in this repository. Blocking by
default would break the common case to defend against a case the same program could
reach directly anyway. **Instead:** an explicit `allow_private: false` option for
programs that fetch caller-supplied URLs, which is the situation where SSRF is real, and
documentation that says so at the point of use.

### 2. A cookie jar — storage policy, not parsing

`parse_cookies`/`parse_set_cookie` already read the wire format. The jar is the part that
decides **whether a cookie may be stored, and whether it may be sent**:

- **Storage** rejects a cookie whose `Domain` is not a suffix of the request host, and
  rejects one whose `Domain` is a **public suffix** (the PSL question). Without the PSL
  the domain rules permit a supercookie; with it they do not.
- **Sending** matches host and path per RFC 6265 §5.1.3–5.1.4, honours `Secure` (never
  over `http:`), and drops the jar's cookies entirely on a cross-origin redirect (rule 1
  above).
- **Expiry** honours `Max-Age` over `Expires`, and evicts on read as well as on write so
  a long-lived jar cannot accumulate dead entries.
- **Limits**, per ADR 0024: a cap on cookies per domain and total, evicting oldest-first.

The PSL is the one piece with a real cost: it is a large, *changing* list, and embedding
a snapshot means shipping something that goes stale. **Recommendation:** embed a
snapshot, expose its date, and treat an unknown suffix conservatively (reject the
`Domain` attribute and store host-only). A host-only cookie is always safe; the PSL only
ever *widens* what is allowed.

`HttpOnly` is deliberately **not** enforced client-side: it is a browser-DOM protection,
and pretending to honour it here would be theatre.

### 3. Per-request timeouts, and the limits that go with them

A request record gains optional `connect_ms`, `read_ms`, and `total_ms`, defaulting to
the current 30s/120s and no total. `total_ms` is the one that actually bounds a hostile
server, because a slow-loris keeps every individual read inside the read timeout. Add,
per ADR 0024:

- a **response body cap** (ureq's 10 MiB today, made explicit and overridable),
- a **header count/size cap**,
- and the redirect limit from rule 1.

A limit that is hit is an ERROR naming the limit, never a truncated body returned as if
complete — a truncated JSON body that parses is a wrong answer, which is worse than a
failure.

### 4. Headers as a type, not a Dict

Header field names are case-insensitive (RFC 9110 §5.1), and HTTP/2 requires them
lowercase on the wire — so the SAME program sees `Content-Type` from an HTTP/1.1 server
and `content-type` from an HTTP/2 one. A Dict cannot express that; the current
lowercase-everything is a correct canonicalisation with a sharp edge, since
`get("Content-Type")` answers `missing` rather than erroring.

**Recommendation:** a `Headers` value whose lookup is case-insensitive, which prints and
iterates like a Dict, and which `{"Content-Type": …}` coerces into on the request side.
`missing` on absence stays (ADR 0020's accessor rule), but it now means *absent* rather
than *possibly mis-cased*.

Also, and this is a real injection vector rather than an ergonomic one: **reject CR/LF in
a header name or value at construction**. A newline in a header value is header
injection — it lets a caller-supplied string add headers or a body of its own. The check
is two lines and belongs where headers are built, not where they are sent.

## Consequences

- A program that fetches a caller-supplied URL is safe by default, and a program that
  wants the unsafe behaviour has to name it. That asymmetry is the whole design.
- The client stops being method-agnostic-by-accident and becomes method-correct on
  purpose, which is what makes QUERY (RFC 10008) work through a redirect.
- A jar is state, and state is where reproducibility goes to die — so the jar is
  **explicit** (a value the program holds and passes), never an ambient process-wide
  default. ADR 0020's reasoning about ordering applies: two runs of one program with the
  same inputs must agree, and an implicit jar makes the second run differ from the first.
- The PSL snapshot is a maintenance obligation with a date on it, and the ADR should be
  revisited rather than the snapshot quietly refreshed.

## Implementation plan

Four independent steps, each shippable and pinned on its own. Ordered by risk retired
per unit of work, not by size.

**Step 1 — Header injection guard + `Headers` type.** (Smallest, and closes a real
injection vector.) Reject CR/LF at header construction, in the one place request headers
are built. Then a `Headers` value with case-insensitive lookup, coerced from a dict
literal, printing as a dict. Pin: mixed-case lookup, an HTTP/2-style all-lowercase
response, and a CRLF-injection attempt refused. No network needed — all of it is
testable against the local `listen` server, as the v0.3.0 QUERY verification was.

**Step 2 — Per-request timeouts and limits.** `connect_ms`/`read_ms`/`total_ms`/
`max_body` on the request record; a distinct error per limit naming which one was hit.
Pin against a local server that stalls deliberately, and assert the error names the
limit rather than returning a short body.

**Step 3 — Redirect policy.** The boundary rules, the method table (including QUERY's
exception to the POST rule), and the `redirects` chain in the response. Pin every rule
against a local server that issues each redirect status, including: credentials stripped
cross-origin, credentials KEPT same-origin, `https`→`http` refused, a loop hitting the
limit, and `307` preserving a body. This is the step that most needs the local-server
harness, and it is why Step 1 builds it.

**Step 4 — Cookie jar.** Storage and send policy on top of the existing parsers, with the
PSL snapshot and its date. Pin the supercookie refusal (`Domain=.co.uk`), `Secure` over
`http:` refused, path matching, expiry eviction, and the cross-origin redirect drop from
Step 3.

Steps 1–3 are self-contained. Step 4 is the largest and depends on Step 3's origin
comparison, so it goes last and can be dropped from a release without leaving anything
half-built.
