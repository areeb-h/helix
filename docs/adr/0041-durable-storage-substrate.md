# ADR 0041 — the durable-storage substrate

**Status:** Accepted & implemented
**Date:** 2026-08-29

## The question

Can a correct storage engine be written *in* Helix?

Before this, no — and not for want of cleverness. The filesystem surface was `mkdir`,
`remove_file`, `file_exists`, `read_dir`, `read_text`, `write_to`, `append_to`. A field
report building a versioned store worked that out precisely and reported it as blocker 9:

> **No `rename`, no `fsync`, no `rmdir`** — so write-temp-then-rename isn't expressible.

## What the constraint produced, and why it still is not enough

The report designed around the gap: every version is written under its own sequence number,
heads are signed, and a reader takes the newest head that *verifies*. A torn write does not
verify, so it is skipped; chunks are content-addressed and written before any head, so a
crash leaves orphans rather than corruption. It was tested three ways — truncated head,
forged head, head signed by the wrong key — and all three fall back correctly.

That is a better design than `rename` alone would have produced, and keeping every head is
exactly what time travel needs. **It is also not durability.** "Verified" there means the
bytes reached the operating system, and a power loss discards the page cache. No amount of
verification above the filesystem can supply a promise the filesystem never made.

## Decision

Nine verbs, added as one set rather than the three that were asked for. **A partial
durability story is worse than none**: a program that calls `fsync` and skips `sync_dir`
believes it committed and, after a crash, did not.

### Atomicity

- **`rename(from, to)`** — the commit primitive. Within one filesystem it either happened or
  did not, and an existing destination is replaced in the same instant. **Refuses to cross a
  filesystem** (`EXDEV`) rather than degrading to copy-then-delete, because that copy is the
  non-atomic window callers came here to avoid.
- **`create_new(path, contents)`** — create only if absent, atomically; `false` if it was
  already there, and then nothing is written. `file_exists` followed by `write_to` is the
  same idea with a race in the middle. Two jobs: a **lock or leader election**, decided by
  the kernel; and a **safe content-addressed write**, where a chunk named by its own hash
  must never be rewritten and `false` means "already stored", which is success.

### Durability

- **`fsync(path)`** — flush a file's bytes to the device and wait.
- **`sync_dir(path)`** — flush a *directory entry*. **The step everyone forgets.** After
  `rename(tmp, final)` the contents can be durable while the rename is not: a crash reverts
  the directory and the commit vanishes even though `fsync` reported success.

So a durable commit is exactly: `write` → `fsync` → `rename` → `sync_dir`.

`sync_dir` **answers `false` where the platform cannot do it** rather than `true`. Windows
exposes no directory flush through the standard library, and returning `true` there would be
a durability claim that cannot be kept — the shape of lie that loses data on exactly one
platform. A caller that needs the guarantee can test the answer.

### Random access

- **`file_size(path)`** — the length in BYTES from metadata. `read_text(p).length()` is
  O(file) *and* counts characters, which is a different number for non-ASCII content.
- **`read_at(path, offset, len)`** — read a slice without reading the file. Every read was
  O(file), so one index lookup paid for the whole dataset. Returns what is there, so a short
  final page is a shorter string rather than an error.
- **`write_at(path, offset, text)`** — update a page in place, returning the BYTE count.
- **`truncate(path, len)`** — reclaim a write-ahead log after a checkpoint; growing
  zero-fills, which is how a page file is preallocated.

### Lifecycle

- **`remove_dir(path)`** — an EMPTY directory, `false` if absent. **Never recursive**: a
  recursive delete is one typo from removing a tree nobody named, and this language will not
  make that a one-liner. Remove the contents with `read_dir` and `remove_file`, which keeps
  the decision at the call site.

All nine are capability-gated (ADR 0021). `fsync` and `sync_dir` are classified **FsWrite**
even though they add no bytes: they exist only to complete a write that already happened, and
a read-only program has nothing to make durable. `file_size` and `read_at` are FsRead, and
`fs-read` does not imply `fs-write` — pinned by a test.

## Why `read_at` returns a String, and the honest limit

A Helix `Str` is UTF-8, so a slice that splits a multi-byte character is **refused by name**
rather than replaced with U+FFFD, which would silently corrupt the byte the caller asked for.

That refusal is correct and it is also the ceiling. **Helix has no `Bytes` type**, so this
substrate supports a *text-structured* store — records aligned to character boundaries,
JSON or delimited chunks — and cannot store arbitrary binary. A page-oriented engine with
packed integers, bitmaps or compressed blocks needs a byte string that is not text. That is
the largest remaining gap and deserves its own ADR; it is a new `Value` variant with
consequences across equality, hashing, printing, JSON and the three engines.

## What this does and does not buy

It makes a **correct, crash-safe** storage engine writable in Helix. It does not, by itself,
make one that beats a mature database, and the difference is worth stating plainly rather
than leaving to inference.

SQLite and PostgreSQL are decades of work on query planning, B-tree concurrency, WAL design,
MVCC and vacuum. Nothing here competes with that, and a native engine should not be measured
against them on OLTP — row-at-a-time transactions with many concurrent writers is the ground
they were built to hold.

Where a native Helix engine has a real structural advantage:

- **Analytical scans.** SQLite and PostgreSQL are row stores; Helix's arrays are already
  packed columnar buffers (`ArrayData::Ints`/`Floats`), so a columnar store needs no
  conversion at either end.
- **No serialization boundary.** A query result IS a DataFrame. Every other engine marshals
  rows across a driver; that cost does not exist here.
- **Versioning as a first-class fact.** None of the three does time travel natively. The
  field report's signed-head design already has it, and keeping every head is cheap.
- **The differential oracle.** Three engines held bit-identical is a correctness instrument
  no database has.

What is still missing beyond `Bytes`, stated so it is not discovered later:

- ~~No advisory locking.~~ **Fixed in the same cycle**, because the gap was worse than it
  first reads: a `create_new` lock file left by a crashed process is indistinguishable from a
  live writer, and every remedy at that level is a guess (a PID that may have been reused, a
  timestamp that may be a long pause, a heartbeat that is one more thing to get wrong).
  `lock_file` / `try_lock_file` take a KERNEL-held lock on an open descriptor, so it is
  released by `release()`, by the handle dropping, by exit, and by SIGKILL. Measured both
  ways: after `kill -9` the kernel lock is free and the lock file still reports busy. They
  are ADVISORY — they exclude other lock takers, not other writers — which is true of file
  locks on every operating system and is stated so it is not discovered during a corruption.
- **No `fsync` on an open handle.** `fsync(path)` reopens the file, which is correct but
  costs an extra `open`, and it cannot flush a handle mid-write.
- **No `O_DIRECT`, no write barriers beyond `fsync`**, so the engine is at the mercy of a
  device that lies about flushing. Every database has this problem; it is named here so the
  guarantee is not overstated.

## Consequences

- Write-temp-then-rename is expressible, and so is the full durable-commit sequence.
- A page-oriented store is possible: `read_at` and `write_at` make access O(page) instead of
  O(file), and `file_size` is O(1).
- `remove_dir` closes the reported blocker; the empty-directory leftovers had no remedy.
- The sandbox still covers everything: a durability primitive that escaped it would be a hole
  in the sandbox.
