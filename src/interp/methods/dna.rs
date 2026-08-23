//! Dna methods — sequences, k-mers, complements, IUPAC policy — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

/// GC fraction of a DNA string, shared by `gc_content`, `at_content`, and `mean_gc` so
/// the three cannot drift. The IUPAC policy lives on `simd::gc_counts`: `S` counts as
/// GC, `W` as non-GC, and the codes ambiguous about GC-ness (`N`, `R Y K M B D H V`)
/// are excluded from numerator and denominator alike. `Ok(None)` means the sequence
/// has no classifiable base — the fraction is unknown, and the caller renders it as
/// `missing` (ADR 0001) rather than a fabricated `0.0`. Errors only on an empty
/// sequence, which is a mistake in the program rather than a condition in the data.
pub(crate) fn dna_gc(s: &str, who: &str, line: usize, col: usize) -> Result<Option<f64>, HelixError> {
    if s.is_empty() {
        return Err(HelixError::new(
            format!("cannot compute `{who}` of an empty sequence"),
            line,
            col,
        ));
    }
    // `Dna` is ASCII (validated + upper-cased at construction), so count raw bytes —
    // AVX2 when available, else the auto-vectorized scalar path.
    let (gc, classified) = crate::simd::gc_counts(s.as_bytes());
    Ok((classified > 0).then(|| gc as f64 / classified as f64))
}

pub(crate) fn dna_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "length" | "count" => {
            if !args.is_empty() {
                return Err(HelixError::new(format!("`{}` takes no arguments", name), line, col));
            }
            Ok(Value::Int(s.len() as i64))
        }
        "gc_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_content` takes no arguments", line, col));
            }
            if s.is_empty() {
                return Err(HelixError::new(
                    "cannot compute `gc_content` of an empty sequence",
                    line,
                    col,
                ));
            }
            // GC fraction over *classifiable* bases — see `simd::gc_counts` for the
            // policy: `S` ("G or C") is GC, `W` ("A or T") is not, and every code that
            // could be either (`N`, `R Y K M B D H V`) is excluded from numerator AND
            // denominator, so `gc_content("GCN") == 1.0`, not 2/3, and `"GCS"` reads
            // 1.0 rather than LOWER than the same sequence without the S. A sequence
            // with no classifiable base has an unknown fraction: `missing` (ADR 0001),
            // because 0.0 here is indistinguishable from a genuinely AT-only answer.
            match dna_gc(s, "gc_content", line, col)? {
                Some(gc) => Ok(Value::Float(gc)),
                None => Ok(Value::Missing),
            }
        }
        "complement" => {
            if !args.is_empty() {
                return Err(HelixError::new("`complement` takes no arguments", line, col));
            }
            Ok(Value::Dna(Rc::new(complement(s))))
        }
        "reverse_complement" => {
            if !args.is_empty() {
                return Err(HelixError::new(
                    "`reverse_complement` takes no arguments",
                    line,
                    col,
                ));
            }
            // One pass, one allocation: write the complement of byte `i` straight into the
            // reversed output slot (`complement(s).chars().rev()` was two passes + two
            // allocations). Byte-reverse equals char-reverse for ASCII (always, for DNA).
            let rc = if s.is_ascii() {
                let lut = complement_lut();
                let bytes = s.as_bytes();
                let n = bytes.len();
                let mut out = vec![0u8; n];
                for (i, &b) in bytes.iter().enumerate() {
                    out[n - 1 - i] = lut[b as usize];
                }
                // SAFETY: ASCII in, LUT maps ASCII→ASCII, so every output byte is valid UTF-8.
                unsafe { String::from_utf8_unchecked(out) }
            } else {
                complement(s).chars().rev().collect()
            };
            Ok(Value::Dna(Rc::new(rc)))
        }
        "find" => {
            arity("find", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find` needs a string or DNA pattern, but got {}", crate::value::with_article(v.type_name())),
                        line,
                        col,
                    ))
                }
            };
            // ACGT is ASCII, so the byte offset is the base offset.
            match s.find(&needle) {
                Some(idx) => Ok(Value::Int(idx as i64)),
                None => Ok(Value::Missing),
            }
        }
        "find_all" => {
            arity("find_all", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find_all` needs a string or DNA pattern, but got {}", crate::value::with_article(v.type_name())),
                        line,
                        col,
                    ))
                }
            };
            if needle.is_empty() {
                return Err(HelixError::new("`find_all` needs a non-empty pattern", line, col)
                    .hint("pass the motif you're scanning for, e.g. `seq.find_all(\"GAATTC\")`."));
            }
            // Every 0-based start position, overlapping allowed (advance by 1 past each
            // hit) — the motif-scan / restriction-site convention. ACGT is ASCII so the
            // byte offset is the base offset, and `str::find` is memchr/Two-Way backed,
            // so this is one native O(n) pass instead of materializing n windows.
            let hay = s.as_str();
            let mut positions = Vec::new();
            let mut start = 0usize;
            while let Some(off) = hay[start..].find(needle.as_str()) {
                let idx = start + off;
                positions.push(idx as i64);
                start = idx + 1;
            }
            Ok(Value::int_array(positions))
        }
        "gc_skew" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_skew` takes no arguments", line, col));
            }
            // The cumulative GC-skew walk: +1 per G, -1 per C, unchanged on A/T/N. The
            // running total at each base — the classic replication-origin signal, whose
            // minimum marks the ori. One native pass replaces a per-base interpreter loop;
            // exact integers (no float drift). An empty sequence yields `[]`.
            let mut acc: i64 = 0;
            let walk: Vec<i64> = s
                .bytes()
                .map(|b| {
                    match b {
                        b'G' => acc += 1,
                        b'C' => acc -= 1,
                        _ => {}
                    }
                    acc
                })
                .collect();
            Ok(Value::int_array(walk))
        }
        "longest_homopolymer" => {
            if !args.is_empty() {
                return Err(HelixError::new("`longest_homopolymer` takes no arguments", line, col));
            }
            // Length of the longest run of a single identical base — a common QC signal
            // (long homopolymers are a sequencer error mode). One byte pass, no allocation;
            // an empty sequence is `0`. `prev = 0` (NUL) never equals an ASCII base, so the
            // first base correctly starts a run of 1.
            let mut best = 0i64;
            let mut run = 0i64;
            let mut prev = 0u8;
            for &b in s.as_bytes() {
                if b == prev {
                    run += 1;
                } else {
                    run = 1;
                    prev = b;
                }
                if run > best {
                    best = run;
                }
            }
            Ok(Value::Int(best))
        }
        "kmers" => {
            // The countable k-mer *spectrum*: only windows of unambiguous ACGT —
            // any window containing `N`/IUPAC is skipped (the Jellyfish/KMC/KmerGo
            // convention), so every emitted k-mer round-trips through `dna()` and is
            // canonicalizable. A sequence shorter than `k` (or empty) yields `[]`.
            let k = kmer_k("kmers", args, line, col)?;
            // DNA is validated ASCII, so windows are byte slices (no `Vec<char>` build,
            // no per-char decode); a window is unambiguous iff every byte is `ACGT`.
            let bytes = s.as_bytes();
            let mut out = Vec::new();
            if k <= bytes.len() {
                window_count_guard("kmers", bytes.len() - k + 1, line, col)?;
                for w in bytes.windows(k) {
                    if w.iter().all(|&b| matches!(b, b'A' | b'C' | b'G' | b'T')) {
                        // SAFETY: a window of an ASCII DNA string is valid UTF-8.
                        out.push(Value::Str(Rc::new(unsafe { String::from_utf8_unchecked(w.to_vec()) })));
                    }
                }
            }
            Ok(Value::array(out))
        }
        "windows" => {
            // Every length-`k` substring, faithfully (ambiguity included) — the
            // sequence is reconstructable from its windows. Shorter than `k` → `[]`.
            let k = kmer_k("windows", args, line, col)?;
            // DNA is validated ASCII → byte-slice windows (no `Vec<char>`, no decode).
            let bytes = s.as_bytes();
            let mut out = Vec::new();
            if k <= bytes.len() {
                let count = bytes.len() - k + 1;
                window_count_guard("windows", count, line, col)?;
                out.reserve(count);
                for w in bytes.windows(k) {
                    // SAFETY: a window of an ASCII DNA string is valid UTF-8.
                    out.push(Value::Str(Rc::new(unsafe { String::from_utf8_unchecked(w.to_vec()) })));
                }
            }
            Ok(Value::array(out))
        }
        "codons" => {
            if !args.is_empty() {
                return Err(HelixError::new("`codons` takes no arguments", line, col));
            }
            // Split into non-overlapping reading-frame-0 triplets, dropping a trailing
            // partial codon (length not a multiple of 3) — the standard codon iteration
            // for a coding sequence, feeding a `codon -> amino acid` lookup. A `Dna` is
            // ASCII, so step the bytes in chunks of 3 (no per-base decode) and emit one
            // string per codon. A sequence shorter than 3 yields `[]`.
            let bytes = s.as_bytes();
            let count = bytes.len() / 3;
            window_count_guard("codons", count, line, col)?;
            let mut out = Vec::with_capacity(count);
            for chunk in bytes.chunks_exact(3) {
                out.push(Value::Str(Rc::new(String::from_utf8_lossy(chunk).into_owned())));
            }
            Ok(Value::array(out))
        }
        "kmer_counts" => {
            // Native 2-bit-packed k-mer spectrum (k ≤ 32): each ACGT window packs
            // into a u64 — no per-window string allocation — counted in a hash map;
            // only the *distinct* k-mers are decoded to strings at the end. Windows
            // spanning N/IUPAC are skipped (same spectrum as `kmers`). Returns
            // (kmer, count) tuples, count desc then k-mer asc. The fast path for
            // `kmers(k).frequencies()`.
            let k = kmer_k("kmer_counts", args, line, col)?;
            if k > 32 {
                return Err(HelixError::new(
                    format!("`kmer_counts` supports k up to 32 (2-bit packed), got {}", k),
                    line,
                    col,
                )
                .hint("for larger k use `kmers(k).frequencies()`."));
            }
            Ok(Value::array(packed_kmer_counts(s, k, false)))
        }
        "canonical_kmer_counts" => {
            // Strand-agnostic k-mer spectrum: a k-mer and its reverse complement are
            // counted together under their *canonical* form (the lexicographically
            // smaller of the two), so coverage from either strand collapses to one
            // entry — the Jellyfish/KMC `--canonical` convention. Same 2-bit-packed
            // counting as `kmer_counts`; the reverse complement is computed directly
            // on the packed code (complement = `bits ^ 3`, then the bases reversed).
            let k = kmer_k("canonical_kmer_counts", args, line, col)?;
            if k > 32 {
                return Err(HelixError::new(
                    format!("`canonical_kmer_counts` supports k up to 32 (2-bit packed), got {}", k),
                    line,
                    col,
                )
                .hint("for larger k, canonicalize `kmers(k)` yourself before `frequencies()`."));
            }
            Ok(Value::array(packed_kmer_counts(s, k, true)))
        }
        "align" => {
            // `seq.align(target[, mode])` — pairwise alignment (ADR 0015). The result
            // is a plain record so it composes with field access and prints normally.
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`align` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("call `seq.align(target)` or `seq.align(target, \"local\")`."));
            }
            let target = match &args[0] {
                Value::Dna(t) => t,
                other => return Err(type_err("align", "a DNA sequence", other, line, col)),
            };
            let mode = match args.get(1) {
                None => crate::align::Mode::Global,
                Some(Value::Str(m)) => match m.as_str() {
                    "global" => crate::align::Mode::Global,
                    "local" => crate::align::Mode::Local,
                    "semiglobal" => crate::align::Mode::Semiglobal,
                    other => {
                        return Err(HelixError::new(
                            format!("unknown alignment mode `{other}`", ),
                            line,
                            col,
                        )
                        .hint("the modes are \"global\" (default), \"local\", and \"semiglobal\"."))
                    }
                },
                Some(other) => return Err(type_err("align", "a mode string", other, line, col)),
            };
            // Cap the dynamic-programming matrix: it is O(n*m) in both time and memory
            // (six matrices over the (n+1)x(m+1) grid), so a pair of very long sequences
            // would exhaust memory. Reads-vs-genes stay far under this; whole-genome
            // alignment is out of scope (ADR 0015).
            // 50M cells: at i64 scores the six DP matrices are ~27 bytes/cell, so this
            // bounds the table near ~1.3 GB (halved from the old i32 cap to match the
            // wider, overflow-proof score type).
            const MAX_ALIGN_CELLS: usize = 50_000_000;
            let cells = s.len().saturating_mul(target.len());
            if cells > MAX_ALIGN_CELLS {
                return Err(HelixError::new(
                    format!(
                        "`align` would build a {}x{} matrix, too large (keep the product under {})",
                        s.len(),
                        target.len(),
                        MAX_ALIGN_CELLS
                    ),
                    line,
                    col,
                )
                .hint("align shorter sequences, or a region of each."));
            }
            let a = crate::align::align(
                s.as_bytes(),
                target.as_bytes(),
                mode,
                crate::align::Scoring::nucleotide(),
            );
            use crate::symbol::Symbol;
            Ok(Value::Record(Rc::new(vec![
                (Symbol::intern("score"), Value::Int(a.score)),
                (Symbol::intern("cigar"), Value::Str(Rc::new(a.cigar))),
                (Symbol::intern("query"), Value::Str(Rc::new(a.x_aligned))),
                (Symbol::intern("target"), Value::Str(Rc::new(a.y_aligned))),
                (Symbol::intern("start"), Value::Int(a.y_start as i64)),
                (Symbol::intern("end"), Value::Int(a.y_end as i64)),
            ])))
        }
        "at_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`at_content` takes no arguments", line, col));
            }
            // AT fraction = 1 − GC fraction, over the same classifiable-base policy —
            // which is what makes `dna("S").at_content()` answer 0.0 (S is never A or
            // T) instead of the old 1.0, and keeps `gc_content + at_content == 1.0`
            // whenever either is a number at all.
            match dna_gc(s, "at_content", line, col)? {
                Some(gc) => Ok(Value::Float(1.0 - gc)),
                None => Ok(Value::Missing),
            }
        }
        // Per-base tally in ONE pass over the sequence (no per-base string allocation):
        // `{A, C, G, T, N}` where `N` collects every non-ACGT base. Access via `.A` etc.
        "base_counts" => {
            if !args.is_empty() {
                return Err(HelixError::new("`base_counts` takes no arguments", line, col));
            }
            // A `Dna` is ASCII (validated + upper-cased at construction), so count raw
            // bytes — no UTF-8 decode. `simd::base_counts` uses AVX2 (32 bases/instr) when
            // available, else a branchless auto-vectorized scalar count; both are exact, so
            // `N` (every non-ACGT base) is the remainder, matching the old `_ => n` arm.
            let bytes = s.as_bytes();
            let (a, c, g, t) = crate::simd::base_counts(bytes);
            let n = bytes.len() as i64 - a - c - g - t;
            use crate::symbol::Symbol;
            Ok(Value::Record(Rc::new(vec![
                (Symbol::intern("A"), Value::Int(a)),
                (Symbol::intern("C"), Value::Int(c)),
                (Symbol::intern("G"), Value::Int(g)),
                (Symbol::intern("T"), Value::Int(t)),
                (Symbol::intern("N"), Value::Int(n)),
            ])))
        }
        // Hamming distance: differing positions between two equal-length sequences, in one
        // pass (no per-base slices). The other sequence may be a `Dna` or a `String`.
        "hamming" => {
            arity("hamming", args, 1, line, col)?;
            let other: &str = match &args[0] {
                Value::Dna(o) => o,
                Value::Str(o) => o,
                v => {
                    return Err(HelixError::new(
                        format!("`hamming` needs a DNA or string sequence, but got {}", crate::value::with_article(v.type_name())),
                        line,
                        col,
                    ))
                }
            };
            // Fast path: both ASCII (always, for a `Dna` receiver vs an ASCII sequence) →
            // compare bytes (the comparison auto-vectorizes, no per-char decode). Falls
            // back to the exact char-based count for any non-ASCII `other`.
            if s.is_ascii() && other.is_ascii() {
                let (sb, ob) = (s.as_bytes(), other.as_bytes());
                if sb.len() != ob.len() {
                    return Err(HelixError::new(
                        format!("`hamming` needs equal-length sequences, got {} and {}", sb.len(), ob.len()),
                        line,
                        col,
                    )
                    .hint("align or trim the sequences to the same length first."));
                }
                let dist = sb.iter().zip(ob).filter(|(x, y)| x != y).count();
                return Ok(Value::Int(dist as i64));
            }
            let (ls, lo) = (s.chars().count(), other.chars().count());
            if ls != lo {
                return Err(HelixError::new(
                    format!("`hamming` needs equal-length sequences, got {ls} and {lo}"),
                    line,
                    col,
                )
                .hint("align or trim the sequences to the same length first."));
            }
            let dist = s.chars().zip(other.chars()).filter(|(x, y)| x != y).count();
            Ok(Value::Int(dist as i64))
        }
        _ => Err(unknown_method(
            "Dna",
            name,
            &crate::registry::methods_of(crate::registry::DNA_METHODS),
            line,
            col,
        )),
    }
}

/// 2-bit-packed k-mer counts (k ≤ 32), as `(kmer, count)` tuples sorted by count
/// desc then k-mer asc. Each ACGT window rolls into a `u64` (A=0 C=1 G=2 T=3) with
/// no allocation; a non-ACGT base breaks the window (the `kmers` spectrum). A string
/// is built only per *distinct* k-mer (decoded at the end), and u64 keys hash far
/// faster than strings — the native fast path for `kmers(k).frequencies()`. Same
/// fixed width means u64 order == lexicographic k-mer order, so sorting by the packed
/// code matches a string sort. When `canonical`, each window is counted under
/// `min(code, reverse_complement(code))`, collapsing the two strands into one entry.
pub(crate) fn packed_kmer_counts(s: &str, k: usize, canonical: bool) -> Vec<Value> {
    let mask: u64 = if k >= 32 { u64::MAX } else { (1u64 << (2 * k)) - 1 };
    let mut code: u64 = 0;
    let mut valid: usize = 0;
    // FxHashMap (fast non-cryptographic hash) — u64 keys hash in a couple of ops,
    // the point of packing vs hashing 4.6M strings.
    let mut counts: rustc_hash::FxHashMap<u64, u64> = rustc_hash::FxHashMap::default();
    for byte in s.bytes() {
        let bits = match byte {
            b'A' => 0u64,
            b'C' => 1,
            b'G' => 2,
            b'T' => 3,
            _ => {
                valid = 0; // ambiguous base — break the window
                continue;
            }
        };
        code = ((code << 2) | bits) & mask;
        valid += 1;
        if valid >= k {
            let key = if canonical { code.min(revcomp_code(code, k)) } else { code };
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    let mut pairs: Vec<(u64, u64)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs
        .into_iter()
        .map(|(c, n)| {
            let mut km = String::with_capacity(k);
            for i in 0..k {
                let b = (c >> (2 * (k - 1 - i))) & 3;
                km.push(b"ACGT"[b as usize] as char);
            }
            Value::Tuple(Rc::new(vec![Value::Str(Rc::new(km)), Value::Int(n as i64)]))
        })
        .collect()
}

/// The reverse complement of a 2-bit-packed `k`-mer code (A=0 C=1 G=2 T=3). Each
/// base is complemented by `bits ^ 3` (A↔T, C↔G) and the bases are emitted in
/// reverse order, so the result is itself a valid `k`-base packed code.
pub(crate) fn revcomp_code(mut code: u64, k: usize) -> u64 {
    let mut rc: u64 = 0;
    for _ in 0..k {
        let base = code & 3;
        rc = (rc << 2) | (base ^ 3);
        code >>= 2;
    }
    rc
}

/// Guard the number of substrings a `kmers`/`windows`/`split` call would emit, so a
/// huge input errors cleanly instead of allocating tens of GB of `Value::Str`.
pub(crate) fn window_count_guard(name: &str, count: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if count > MAX_ELEMENTS {
        return Err(HelixError::new(
            format!("`{name}` would produce {count} substrings, too many to hold in memory"),
            line,
            col,
        )
        .hint("use a longer k, a shorter input, or `kmer_counts(k)` for the spectrum."));
    }
    Ok(())
}

pub(crate) fn kmer_k(name: &str, args: &[Value], line: usize, col: usize) -> Result<usize, HelixError> {
    arity(name, args, 1, line, col)?;
    let k = as_int(&args[0], name, line, col)?;
    if k <= 0 {
        return Err(HelixError::new(
            format!("`{}` needs a positive length, got {}", name, k),
            line,
            col,
        ));
    }
    Ok(k as usize)
}

/// A valid (uppercase) IUPAC nucleotide code: the 4 bases, the 10 two/three-fold
/// ambiguity codes, and `N` (any base). This is the alphabet `dna()` accepts and
/// `read_fasta`/`read_fastq` already produce, so the two paths agree.
pub(crate) fn is_iupac_dna(c: char) -> bool {
    matches!(
        c,
        'A' | 'C' | 'G' | 'T' | 'R' | 'Y' | 'S' | 'W' | 'K' | 'M' | 'B' | 'D' | 'H' | 'V' | 'N'
    )
}

/// IUPAC complement of one (uppercase) base. Ambiguity codes complement to the
/// code for the complementary base set (`R`=A/G → `Y`=C/T, etc.); `S`/`W`/`N` are
/// self-complementary. Unknown chars pass through unchanged (defensive).
pub(crate) fn iupac_complement(c: char) -> char {
    match c {
        'A' => 'T',
        'T' => 'A',
        'C' => 'G',
        'G' => 'C',
        'R' => 'Y',
        'Y' => 'R',
        'K' => 'M',
        'M' => 'K',
        'B' => 'V',
        'V' => 'B',
        'D' => 'H',
        'H' => 'D',
        'S' => 'S',
        'W' => 'W',
        'N' => 'N',
        other => other,
    }
}

/// A 256-entry byte lookup table for the IUPAC complement: each mapped uppercase code
/// (A↔T, C↔G, R↔Y, K↔M, B↔V, D↔H; S/W/N self-complementary) to its complement, identity
/// for every other byte. DNA is validated ASCII, so a per-byte map is exactly equivalent
/// to the per-char [`iupac_complement`] but branchless and vectorizable. Built once.
pub(crate) fn complement_lut() -> &'static [u8; 256] {
    static LUT: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 256];
        for (i, e) in t.iter_mut().enumerate() {
            *e = i as u8; // identity for every unmapped byte (matches `other => other`)
        }
        for (k, v) in [
            (b'A', b'T'), (b'T', b'A'), (b'C', b'G'), (b'G', b'C'), (b'R', b'Y'), (b'Y', b'R'),
            (b'K', b'M'), (b'M', b'K'), (b'B', b'V'), (b'V', b'B'), (b'D', b'H'), (b'H', b'D'),
            (b'S', b'S'), (b'W', b'W'), (b'N', b'N'),
        ] {
            t[k as usize] = v;
        }
        t
    })
}

pub(crate) fn complement(s: &str) -> String {
    // Fast path for ASCII (always, for a validated DNA string): a branchless byte LUT in
    // one pass. The fallback keeps exact behaviour for any non-ASCII input.
    if s.is_ascii() {
        let lut = complement_lut();
        let bytes: Vec<u8> = s.bytes().map(|b| lut[b as usize]).collect();
        // SAFETY: input is ASCII and the LUT maps each ASCII byte to another ASCII byte,
        // so every output byte is valid single-byte UTF-8.
        unsafe { String::from_utf8_unchecked(bytes) }
    } else {
        s.chars().map(iupac_complement).collect()
    }
}


