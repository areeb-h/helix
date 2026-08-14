//! SIMD-accelerated byte counting for the bio composition kernels (`base_counts`,
//! `gc_content`). On x86-64 with AVX2 it counts 32 bases per instruction; everywhere
//! else (and on a CPU without AVX2) a branchless scalar loop the compiler auto-vectorizes
//! to the baseline ISA. The AVX2 path is **verified byte-identical** to the scalar count
//! (`tests::avx2_matches_scalar`, fuzzed around the 32-byte and flush boundaries with
//! lowercase / ambiguity / non-DNA bytes), so it is a pure speedup with no semantics
//! change — and the rest of the engine never sees it (both produce the same counts).

/// Count `A`/`C`/`G`/`T` (uppercase ASCII) in `bytes` → `(a, c, g, t)`. Every other byte
/// (`N`, IUPAC, lowercase, …) is none of the four, so the caller derives `N` as the
/// remainder. Identical to `bytes.iter().filter(|b| **b == X).count()` for each base.
pub fn base_counts(bytes: &[u8]) -> (i64, i64, i64, i64) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: the runtime check guarantees AVX2 is present.
            return unsafe { base_counts_avx2(bytes) };
        }
    }
    base_counts_scalar(bytes)
}

/// Count GC-definite and GC-classifiable bases in `bytes` → `(gc, classified)`.
///
/// The policy (stated in the bio docs and pinned by `dna_iupac_arithmetic_is_correct`):
/// a base counts toward the GC fraction iff its identity *with respect to GC-ness* is
/// certain. `S` is BY DEFINITION "G or C" — GC either way — so it belongs in the
/// numerator; `W` ("A or T") is definitely not GC, so it belongs in the denominator
/// only. Every code that could be either (`N`, and the partial codes
/// `R Y K M B D H V`) is excluded from BOTH — the same rule `N` always had, extended
/// to the codes it was arbitrarily not applied to. Counting `S` as non-GC while
/// leaving it in the denominator — the old behaviour — was arithmetically identical
/// to declaring it an A or a T.
///
/// The caller's GC fraction is `gc / classified`, and a sequence with `classified == 0`
/// has an UNKNOWN fraction — `missing`, per ADR 0001, not `0.0`.
pub fn gc_counts(bytes: &[u8]) -> (i64, i64) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { gc_counts_avx2(bytes) };
        }
    }
    gc_counts_scalar(bytes)
}

/// Branchless scalar count — the portable path (auto-vectorizes to the baseline ISA) and
/// the oracle the AVX2 path is tested against.
fn base_counts_scalar(bytes: &[u8]) -> (i64, i64, i64, i64) {
    let (mut a, mut c, mut g, mut t) = (0i64, 0i64, 0i64, 0i64);
    for &b in bytes {
        a += (b == b'A') as i64;
        c += (b == b'C') as i64;
        g += (b == b'G') as i64;
        t += (b == b'T') as i64;
    }
    (a, c, g, t)
}

fn gc_counts_scalar(bytes: &[u8]) -> (i64, i64) {
    let (mut gc, mut classified) = (0i64, 0i64);
    for &b in bytes {
        gc += (b == b'G' || b == b'C' || b == b'S') as i64;
        classified +=
            (b == b'A' || b == b'C' || b == b'G' || b == b'T' || b == b'S' || b == b'W') as i64;
    }
    (gc, classified)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn base_counts_avx2(bytes: &[u8]) -> (i64, i64, i64, i64) {
    use std::arch::x86_64::*;
    unsafe {
        let (va, vc, vg, vt) = (
            _mm256_set1_epi8(b'A' as i8),
            _mm256_set1_epi8(b'C' as i8),
            _mm256_set1_epi8(b'G' as i8),
            _mm256_set1_epi8(b'T' as i8),
        );
        let (mut ta, mut tc, mut tg, mut tt) = (0i64, 0i64, 0i64, 0i64);
        let n = bytes.len();
        let mut i = 0usize;
        // Per-lane i8 counters saturate the byte at 255, so flush every 255 chunks.
        while i + 32 <= n {
            let (mut ca, mut cc, mut cg, mut ct) =
                (_mm256_setzero_si256(), _mm256_setzero_si256(), _mm256_setzero_si256(), _mm256_setzero_si256());
            let mut blk = 0;
            while blk < 255 && i + 32 <= n {
                let chunk = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);
                // cmpeq → 0xFF (-1) per match; subtracting adds 1 per match per lane.
                ca = _mm256_sub_epi8(ca, _mm256_cmpeq_epi8(chunk, va));
                cc = _mm256_sub_epi8(cc, _mm256_cmpeq_epi8(chunk, vc));
                cg = _mm256_sub_epi8(cg, _mm256_cmpeq_epi8(chunk, vg));
                ct = _mm256_sub_epi8(ct, _mm256_cmpeq_epi8(chunk, vt));
                i += 32;
                blk += 1;
            }
            ta += hsum_epu8(ca);
            tc += hsum_epu8(cc);
            tg += hsum_epu8(cg);
            tt += hsum_epu8(ct);
        }
        while i < n {
            let b = bytes[i];
            ta += (b == b'A') as i64;
            tc += (b == b'C') as i64;
            tg += (b == b'G') as i64;
            tt += (b == b'T') as i64;
            i += 1;
        }
        (ta, tc, tg, tt)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gc_counts_avx2(bytes: &[u8]) -> (i64, i64) {
    use std::arch::x86_64::*;
    unsafe {
        let (va, vc, vg, vt, vs, vw) = (
            _mm256_set1_epi8(b'A' as i8),
            _mm256_set1_epi8(b'C' as i8),
            _mm256_set1_epi8(b'G' as i8),
            _mm256_set1_epi8(b'T' as i8),
            _mm256_set1_epi8(b'S' as i8),
            _mm256_set1_epi8(b'W' as i8),
        );
        let (mut tgc, mut tcl) = (0i64, 0i64);
        let n = bytes.len();
        let mut i = 0usize;
        while i + 32 <= n {
            let (mut cgc, mut ccl) = (_mm256_setzero_si256(), _mm256_setzero_si256());
            let mut blk = 0;
            while blk < 255 && i + 32 <= n {
                let chunk = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);
                // GC-definite: G, C, or S ("G or C" — GC either way).
                let is_gc = _mm256_or_si256(
                    _mm256_or_si256(
                        _mm256_cmpeq_epi8(chunk, vg),
                        _mm256_cmpeq_epi8(chunk, vc),
                    ),
                    _mm256_cmpeq_epi8(chunk, vs),
                );
                // Classifiable: the GC-definite set plus A, T, and W ("A or T").
                let is_at = _mm256_or_si256(
                    _mm256_or_si256(
                        _mm256_cmpeq_epi8(chunk, va),
                        _mm256_cmpeq_epi8(chunk, vt),
                    ),
                    _mm256_cmpeq_epi8(chunk, vw),
                );
                cgc = _mm256_sub_epi8(cgc, is_gc);
                ccl = _mm256_sub_epi8(ccl, _mm256_or_si256(is_gc, is_at));
                i += 32;
                blk += 1;
            }
            tgc += hsum_epu8(cgc);
            tcl += hsum_epu8(ccl);
        }
        while i < n {
            let b = bytes[i];
            tgc += (b == b'G' || b == b'C' || b == b'S') as i64;
            tcl +=
                (b == b'A' || b == b'C' || b == b'G' || b == b'T' || b == b'S' || b == b'W') as i64;
            i += 1;
        }
        (tgc, tcl)
    }
}

/// Sum the 32 unsigned bytes of a vector: `sad_epu8` against zero gives four 64-bit
/// partial sums (one per 8-byte group); fold them to a scalar.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum_epu8(v: std::arch::x86_64::__m256i) -> i64 {
    use std::arch::x86_64::*;
    let sad = _mm256_sad_epu8(v, _mm256_setzero_si256());
    let lo = _mm256_castsi256_si128(sad);
    let hi = _mm256_extracti128_si256::<1>(sad);
    let s = _mm_add_epi64(lo, hi);
    _mm_extract_epi64::<0>(s) + _mm_extract_epi64::<1>(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avx2_matches_scalar() {
        // Lengths around the 32-byte vector and 255-chunk flush boundaries; the alphabet
        // mixes ACGT with N, IUPAC, lowercase, and non-DNA bytes so every "not a base"
        // path is exercised.
        let mut rng = 0x1234_5678_9abc_def0u64;
        let alphabet = b"ACGTNacgtRYSWKMnX. ";
        for &len in &[0usize, 1, 5, 31, 32, 33, 63, 64, 65, 127, 128, 255, 256, 257, 8161, 8192, 8224]
        {
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v.push(alphabet[(rng as usize) % alphabet.len()]);
            }
            let bc = base_counts_scalar(&v);
            let gc = gc_counts_scalar(&v);
            assert_eq!(base_counts(&v), bc, "base_counts dispatch != scalar (len {len})");
            assert_eq!(gc_counts(&v), gc, "gc_counts dispatch != scalar (len {len})");
            #[cfg(target_arch = "x86_64")]
            if is_x86_feature_detected!("avx2") {
                assert_eq!(unsafe { base_counts_avx2(&v) }, bc, "base_counts AVX2 != scalar (len {len})");
                assert_eq!(unsafe { gc_counts_avx2(&v) }, gc, "gc_counts AVX2 != scalar (len {len})");
            }
        }
    }
}
