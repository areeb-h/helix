# ADR 0042 — `Bytes`: the type a `String` cannot be

**Status:** Accepted & implemented
**Date:** 2026-08-30

## The question

ADR 0041 made a correct, crash-safe storage engine writable in Helix, and named its own
ceiling in the same breath:

> A Helix `Str` is UTF-8, so a slice that splits a multi-byte character is **refused by
> name** … That refusal is correct and it is also the ceiling. **Helix has no `Bytes`
> type**, so this substrate supports a *text-structured* store and cannot store arbitrary
> binary.

A packed integer, a bitmap, a compressed block and a hash digest are all "not text". Without
a byte string, `read_at` refuses exactly the pages a real engine stores, and the substrate is
a store for JSON and delimited records only.

## Decision

`Value::Bytes(Rc<Vec<u8>>)` — an immutable byte string, ordered lexicographically by byte.

**Widening `Str` was never the alternative.** If a `Str` could hold arbitrary bytes, every
string operation would have to answer "what if this is not text": `upper()`, `chars()`,
`length()`, `split()`, every regex verb. Somewhere the answer would be wrong, and it would be
wrong silently. Two types means each one's operations are total.

### The surface

It mirrors `String` wherever the operation means the same thing — `length`/`count`,
`is_empty`, `take`/`drop`, `slice`, `concat`, `write_to`/`append_to` — so a reader who knows
one knows the other. The methods that differ are the ones where text and bytes genuinely do:

- **`byte_at(i)`** answers an `Int` in 0..=255 where `char_at` answers a one-character
  string, and it is O(1) where `char_at` is O(i) — a byte index needs no decoding. Past the
  end is `missing`, not `0`: an out-of-range read has no honest answer (ADR 0001).
- **`to_string()`** can FAIL, which a string's identity cannot. It refuses by name rather
  than substituting U+FFFD, because that would silently change the data on the way out.
- **`to_hex()` / `to_base64()`** never fail, and are the answer when the bytes are not text.

In: `read_bytes(path)`, `read_bytes_at(path, off, len)`, `from_hex(s)`, `from_base64(s)`,
`"…".to_bytes()`. Out: `write_to`, `append_to`, `write_at`, and the two encoders.

### Printed as hex, in full

`b"00ff10"`. **Never truncated**, even for a large value. An elision would hide exactly the
byte a reader is hunting, and this project treats printed output as a frozen format — a `…`
would be a lie that could not be removed without a versioned event. A large `Bytes` prints
large, which is already true of a large array.

One implementation backs both the printed form and `to_hex()`, so they cannot disagree.

### Ordered, and why that is not decoration

Lexicographic by byte — the order a key index is built on, and the same order `to_hex()`
produces. So `a < b` and `a.to_hex() < b.to_hex()` always agree, which is what makes hex an
honest stand-in wherever `Bytes` is not accepted yet.

`sort`, `min` and `max` accept them for the same reason: a type that compares with `<` but
cannot be sorted is a runtime/library split of exactly the kind this project treats as a bug.

## Deliberately out of the first cut

Named here rather than left to be discovered:

- **Not a dict key.** `dict().insert(from_hex("00"), 1)` is refused **by name** ("a dict key
  must be an int, string, bool, or DNA sequence, not a Bytes") rather than accepted and lost.
  Use `to_hex()`, which preserves the ordering, so this is a keystroke rather than a
  semantic compromise.
- **No JSON form.** `to_json` refuses with "can't serialize a Bytes to JSON". Base64 in JSON
  would be a guess about the reader's intent, and it does not round-trip: the value would
  come back a `Str`. `to_base64()` at the call site says what happened.
- **Not a DataFrame column.** A binary column is a real feature and a separate decision.
- **Runtime-typed to the checker**, like `Dict` and `Net`: the checker sees `Unknown`, so
  method resolution happens when it runs. Arity and argument types are still checked, which
  is where most mistakes actually are.

## Consequences

- A page-oriented **binary** store is possible: `read_bytes_at` reads a page that `read_at`
  must refuse, and `Bytes.write_at` updates one in place.
- Bytes that are not valid UTF-8 survive a round trip through a file, verified end to end
  and on all three engines.
- The remaining gap from ADR 0041 is now only durability's hardware end (no `O_DIRECT`, no
  defence against a device that lies about flushing) and `fsync` on an open handle.

## A note on how this was checked

Adding a `Value` variant produced **no** exhaustive-match errors — every site that could
have needed an arm had a catch-all. That is the dangerous shape, not the safe one, so each
catch-all path was probed by hand rather than assumed: dict key, JSON, `+`, array and record
printing, sorting, and all three engines. Two came back wrong and were fixed (`sort` refused
a type that compares; the checker did not know `String.to_bytes`, producing the memorable
*"type String has no method `to_bytes` — did you mean `to_bytes`?"*), and the rest already
refused correctly.
