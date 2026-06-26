# ADR 0015 — Sequence alignment

- **Status:** Proposed
- **Date:** 2026-06-26
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0003 — Collection API](0003-collection-api.md),
  [ADR 0011 — Core/stdlib boundary](0011-core-stdlib-boundary.md),
  [ADR 0001 — Missing data](0001-missing-data.md)

## Context

Pairwise sequence alignment — scoring how two sequences best correspond under
substitutions and gaps — is the most-used primitive in computational biology after
reading files. It underlies read mapping, variant calling, homology search, primer
design, and QC. The genomics flagship (see the positioning) already reads sequences
(`read_fasta`/`read_fastq`), variants (`read_vcf`/`read_bcf`), and alignments
(`read_sam`/`read_bam`) into Helix values; what it cannot yet do is *compute* an
alignment between two sequences.

The Helix principles that constrain the answer:

- **One obvious way.** One alignment entry point with sane defaults, not a matrix of
  near-duplicate functions.
- **Stand on the ecosystem for hard parsing; own simple algorithms.** The established
  pattern is already visible in the code: binary/compressed file formats are delegated
  to `noodles`/`needletail` (genuinely hard, security-sensitive), while *algorithms*
  over sequences are hand-rolled in Rust — `gc_content`, `kmers`, the 2-bit-packed
  `kmer_counts`/`canonical_kmer_counts`. Alignment is an algorithm, not a parser.
- **No new value type unless it earns its keep.** Records already model heterogeneous
  results (ADR 0003).
- **Memory-safe, self-contained, lean core** (ADR 0011): every dependency is justified
  against the "samtools speed without the segfaults, no system libs" pitch.
- **`missing`-aware, great errors** (ADR 0001).

## Prior approaches and their documented shortcomings

| Tool | Approach | Documented pain |
|------|----------|-----------------|
| EMBOSS `needle`/`water` | Separate CLI per mode (global/local) | Two tools, file-in/file-out, no library ergonomics; mode is chosen by *which binary you run* — the opposite of one obvious way. |
| Biopython `pairwise2` | One function, scores passed as a long positional/keyword soup | Officially **deprecated** for being slow and hard to read; replaced by `PairwiseAligner` precisely because the API sprawled. |
| Biopython `Align.PairwiseAligner` | Stateful object, dozens of mutable attributes (mode, open/extend/end gaps, substitution matrix) | Powerful but heavyweight; the configuration surface is large and order-dependent. |
| scikit-bio | Returns a rich `TabularMSA`/alignment object | Couples alignment to a whole alignment-object model the user must learn. |
| parasail / parasailors | SIMD C library with Rust bindings | Fast, but a **C system dependency** — exactly the "can't find the .so / segfault" footgun Helix exists to avoid. |
| `rust-bio` (`bio` crate) | Pure-Rust `alignment::pairwise::Aligner` (Gotoh affine gaps, global/local/semiglobal) | Correct and battle-tested, but ships as a **30-non-optional-dependency monolith** — paid in full to use two DP functions. |

Two lessons recur: (1) mode-as-separate-tool and score-as-argument-soup both fight
readability; (2) the dependency/runtime cost of a full toolkit is real when all you
need is the core DP.

## Decision

**Add a hand-rolled, pure-Rust pairwise aligner — Gotoh affine-gap dynamic
programming — exposed as a single `Dna` method with an optional mode, returning an
ordinary record. No new dependency; no new value type.**

### Surface

```helix
query = dna("ACGTACGT")
target = dna("ACGAACGT")

a = query.align(target)            # global (Needleman–Wunsch), the default
b = query.align(target, "local")   # local (Smith–Waterman)
c = query.align(target, "semiglobal")  # global in query, free end-gaps in target
```

`align(target[, mode])` is a method on a `Dna` receiver. `mode` is an optional
string — `"global"` (default), `"local"`, or `"semiglobal"` — mirroring the
optional-second-argument shape already used by `read_vcf(path[, region])` this cycle.

### Result

A plain record (no new type), so it composes with field access and prints like any
record:

```helix
a.score      # Int — the optimal alignment score
a.cigar      # Str — SAM-style, e.g. "3M1X4M" (same op→char rendering as read_sam)
a.query      # Str — gapped aligned query,  e.g. "ACGTACGT"
a.target     # Str — gapped aligned target, e.g. "ACGAACGT"
a.start      # Int — 0-based start of the alignment in the target
a.end        # Int — 0-based end (exclusive) of the alignment in the target
```

`query`/`target` are `Str` (not `Dna`) because they carry gap characters (`-`), which
are not valid bases. For `global` the span is the whole target (`start = 0`,
`end = target.length`); for `local`/`semiglobal` it is the aligned sub-range.

### Scoring (v1)

Sensible nucleotide defaults — **match `+1`, mismatch `−1`, gap-open `−5`,
gap-extend `−1`** (affine gaps; a single gap costs open+extend). Custom scoring is
deferred (see Open questions) until named arguments exist, so the API does not grow a
positional score-soup we would regret.

## Rationale

- **Alignment is an algorithm, and Helix owns its algorithms.** The same judgment that
  made `kmer_counts` a 40-line hand-rolled kernel rather than a `rust-bio` call applies
  here: Gotoh's affine-gap DP is ~200 well-understood lines. The "delegate to the
  ecosystem" rule was forged for *parsing* (BGZF/BAM/CRAM — hard, adversarial input);
  it does not earn a 30-crate dependency for a textbook recurrence.
- **It keeps the core lean and self-contained** (ADR 0011): no new transitive deps, no
  compile-time tax, no supply-chain surface — consistent with "no system libs."
- **Full control of the result.** We render the CIGAR with the *same* op→character
  mapping as the SAM/BAM reader (`M/I/D/N/S/H/P/=/X`), keep it `missing`-aware, and use
  Helix's own error style — none of which we would own behind an external API.
- **One obvious way.** One method, mode as a value, defaults that just work; local vs
  global is an argument, not a different function or a different binary.
- **Records, not a new type** (ADR 0003): the result is data the existing verbs already
  handle; nothing new to learn.
- **Room to grow without a rewrite.** Owning the kernel lets us add banding, linear-space
  (Hirschberg), or SIMD later on our terms — or adopt `rust-bio` behind the *same*
  surface if a future need (protein matrices, MSA) justifies the weight.

## Rejected alternatives

- **Delegate to `rust-bio`** — rejected for v1: 30 non-optional transitive dependencies
  to call two DP functions, against the lean-core/self-contained commitment, for an
  algorithm simple enough to own. Reconsider if/when advanced needs (substitution
  matrices, multiple-sequence alignment, SIMD-banded) make the toolkit worth its weight;
  the chosen surface (`align`) can sit on top of it unchanged.
- **A C SIMD library (parasail)** — rejected: a system dependency, the precise footgun
  Helix avoids.
- **Separate `align_global`/`align_local` methods** — rejected: mode-as-name multiplies
  near-duplicate entry points; mode-as-argument is the one-obvious-way form.
- **A dedicated `Alignment` value type** — rejected for v1: a record already carries
  score/CIGAR/aligned strings/span and composes with field access; a bespoke type earns
  its keep only once it needs methods of its own.
- **Configurable scoring in v1 via positional args** — deferred: a positional
  score-soup is exactly the Biopython `pairwise2` mistake; wait for named arguments.

## Consequences

- **Easier:** read→align→inspect end to end in one language (e.g. align a read's `seq`
  against a reference window from `read_fasta`); the CIGAR is consistent across the
  aligner and the SAM/BAM reader.
- **Harder / committed to:** we now own an alignment kernel — its correctness (covered by
  differential tests against hand-worked cases and known scores), and any future
  performance work (banding/linear-space) is ours. v1 is `O(n·m)` time and space, fine
  for reads-vs-genes; whole-chromosome alignment is out of scope by design.
- **Surface stability:** `align(target[, mode])` and the result record's field names are
  the committed contract; scoring configuration will be *added* (a later argument), not
  reshaped.

## Open questions

- **Custom scoring** — match/mismatch and gap costs, and protein substitution matrices
  (BLOSUM/PAM), once named arguments land. Likely a scoring record argument.
- **Protein / RNA receivers** — generalize beyond `Dna` once the RNA/protein type model
  exists (its own ADR).
- **`pretty()` rendering** — a human-readable stacked alignment view (query over target
  with match bars), as a follow-up to the record result.
- **Linear-space / banded variants** for longer inputs, if profiles demand it.
