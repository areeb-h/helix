//! Pairwise sequence alignment — a hand-rolled Gotoh affine-gap dynamic program
//! (ADR 0015). Global (Needleman–Wunsch), local (Smith–Waterman), and semiglobal
//! modes over byte sequences. Pure Rust, no dependencies: alignment is a textbook
//! algorithm, so Helix owns it (the same judgment as the k-mer kernels), reserving
//! crate delegation for the genuinely hard parsing (noodles).
//!
//! Three states per cell — `M` (the last column aligns a base of each sequence),
//! `X` (a gap in the target, consuming the query), and `Y` (a gap in the query,
//! consuming the target) — give affine gap costs: opening a gap costs
//! `gap_open + gap_extend`, each further base `gap_extend`. Time and space are
//! `O(n·m)`; fine for reads-vs-genes, not whole chromosomes (ADR 0015).

/// A sentinel "unreachable" score with headroom so repeated additions never wrap.
/// `i64` (not `i32`): with custom weights up to ±10⁶ and an alignment path up to the
/// cell cap, an `i32` accumulator could overflow and silently corrupt the score.
const NEG_INF: i64 = i64::MIN / 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Align both sequences end to end (Needleman–Wunsch).
    Global,
    /// Best-scoring local sub-alignment (Smith–Waterman).
    Local,
    /// Whole query, free leading/trailing gaps in the target (fit a read into a ref).
    Semiglobal,
}

/// Affine-gap scoring. `gap_open`/`gap_extend` are penalties (negative); a gap of
/// length `L` costs `gap_open + L * gap_extend`.
#[derive(Clone, Copy)]
pub struct Scoring {
    pub match_: i64,
    pub mismatch: i64,
    pub gap_open: i64,
    pub gap_extend: i64,
}

impl Scoring {
    /// Default nucleotide scoring (ADR 0015): match +1, mismatch −1, gap-open −5,
    /// gap-extend −1.
    pub fn nucleotide() -> Self {
        Scoring { match_: 1, mismatch: -1, gap_open: -5, gap_extend: -1 }
    }
}

/// The result of an alignment: the score, a (run-length) extended CIGAR
/// (`=`/`X`/`I`/`D`, the same op characters the SAM reader renders), the two
/// gap-padded sequences, and the `[start, end)` span covered in the target.
pub struct Alignment {
    pub score: i64,
    pub cigar: String,
    pub x_aligned: String,
    pub y_aligned: String,
    pub y_start: usize,
    pub y_end: usize,
}

/// Pick the maximum of three scores, returning it and which state won (0=M,1=X,2=Y).
/// Ties prefer M, then X — keeping diagonal (aligned) moves over gaps.
fn max3(a: i64, b: i64, c: i64) -> (i64, u8) {
    if a >= b && a >= c {
        (a, 0)
    } else if b >= c {
        (b, 1)
    } else {
        (c, 2)
    }
}

/// Align query `x` against target `y` under `mode` and `sc` (Gotoh affine gaps).
pub fn align(x: &[u8], y: &[u8], mode: Mode, sc: Scoring) -> Alignment {
    let n = x.len();
    let m = y.len();
    let w = m + 1;
    let size = (n + 1) * w;

    // Score matrices and per-cell traceback pointers (predecessor state; 3 = origin).
    let mut mm = vec![NEG_INF; size];
    let mut xx = vec![NEG_INF; size];
    let mut yy = vec![NEG_INF; size];
    let mut tm = vec![3u8; size];
    let mut tx = vec![3u8; size];
    let mut ty = vec![3u8; size];

    let first_gap = sc.gap_open + sc.gap_extend;
    mm[0] = 0;

    // First column: query consumed against gaps in the target (X state).
    for i in 1..=n {
        let idx = i * w;
        let open = mm[(i - 1) * w] + first_gap;
        let ext = xx[(i - 1) * w] + sc.gap_extend;
        let (v, from) = if open >= ext { (open, 0) } else { (ext, 1) };
        xx[idx] = v;
        tx[idx] = from;
    }
    // First row: target consumed against gaps in the query (Y state).
    for j in 1..=m {
        let open = mm[j - 1] + first_gap;
        let ext = yy[j - 1] + sc.gap_extend;
        let (v, from) = if open >= ext { (open, 0) } else { (ext, 2) };
        yy[j] = v;
        ty[j] = from;
    }
    // Semiglobal: leading gaps in the target are free, so the query may start
    // anywhere in it — seed the whole Y first row at zero (origin).
    if mode == Mode::Semiglobal {
        for j in 0..=m {
            yy[j] = 0;
            ty[j] = 3;
        }
    }

    for i in 1..=n {
        for j in 1..=m {
            let idx = i * w + j;
            let diag = (i - 1) * w + (j - 1);
            let up = (i - 1) * w + j;
            let left = i * w + (j - 1);

            // M: align x[i-1] with y[j-1] (match or mismatch), from any state.
            let s = if x[i - 1].eq_ignore_ascii_case(&y[j - 1]) { sc.match_ } else { sc.mismatch };
            let (best, from) = max3(mm[diag], xx[diag], yy[diag]);
            mm[idx] = best + s;
            tm[idx] = from;
            // Local: never carry a negative prefix — reset to an empty alignment.
            if mode == Mode::Local && mm[idx] < 0 {
                mm[idx] = 0;
                tm[idx] = 3;
            }

            // X: gap in the target, consuming the query.
            let xo = mm[up] + first_gap;
            let xe = xx[up] + sc.gap_extend;
            if xo >= xe {
                xx[idx] = xo;
                tx[idx] = 0;
            } else {
                xx[idx] = xe;
                tx[idx] = 1;
            }

            // Y: gap in the query, consuming the target.
            let yo = mm[left] + first_gap;
            let ye = yy[left] + sc.gap_extend;
            if yo >= ye {
                yy[idx] = yo;
                ty[idx] = 0;
            } else {
                yy[idx] = ye;
                ty[idx] = 2;
            }
        }
    }

    // Choose the end cell and starting state by mode.
    let (mut i, mut j, mut state, score) = match mode {
        Mode::Global => {
            let (s, st) = max3(mm[n * w + m], xx[n * w + m], yy[n * w + m]);
            (n, m, st, s)
        }
        Mode::Local => {
            // The highest M cell anywhere.
            let (mut bi, mut bj, mut best) = (0usize, 0usize, 0i64);
            for ii in 0..=n {
                for jj in 0..=m {
                    if mm[ii * w + jj] > best {
                        best = mm[ii * w + jj];
                        bi = ii;
                        bj = jj;
                    }
                }
            }
            (bi, bj, 0u8, best)
        }
        Mode::Semiglobal => {
            // Whole query consumed (row n); free trailing target gaps → best of the
            // last row across M and X.
            let (mut bj, mut bst, mut best) = (0usize, 0u8, NEG_INF);
            for jj in 0..=m {
                if mm[n * w + jj] >= best {
                    best = mm[n * w + jj];
                    bj = jj;
                    bst = 0;
                }
                if xx[n * w + jj] > best {
                    best = xx[n * w + jj];
                    bj = jj;
                    bst = 1;
                }
            }
            (n, bj, bst, best)
        }
    };

    let y_end = j;

    // Walk the traceback, emitting ops and gap-padded columns (reversed; flipped below).
    let mut ops: Vec<u8> = Vec::new();
    let mut xa: Vec<u8> = Vec::new();
    let mut ya: Vec<u8> = Vec::new();
    loop {
        let idx = i * w + j;
        match mode {
            Mode::Local => {
                if state == 0 && tm[idx] == 3 {
                    break; // reached the empty-prefix origin
                }
            }
            Mode::Semiglobal => {
                if i == 0 {
                    break; // query consumed; any remaining target is a free leading gap
                }
            }
            Mode::Global => {
                if i == 0 && j == 0 {
                    break;
                }
            }
        }
        match state {
            0 => {
                let op = if x[i - 1].eq_ignore_ascii_case(&y[j - 1]) { b'=' } else { b'X' };
                ops.push(op);
                xa.push(x[i - 1]);
                ya.push(y[j - 1]);
                state = tm[idx];
                i -= 1;
                j -= 1;
            }
            1 => {
                ops.push(b'I');
                xa.push(x[i - 1]);
                ya.push(b'-');
                state = tx[idx];
                i -= 1;
            }
            _ => {
                ops.push(b'D');
                xa.push(b'-');
                ya.push(y[j - 1]);
                state = ty[idx];
                j -= 1;
            }
        }
    }

    let y_start = j;
    ops.reverse();
    xa.reverse();
    ya.reverse();

    // Run-length-encode the op stream into an extended CIGAR.
    let mut cigar = String::new();
    let mut k = 0;
    while k < ops.len() {
        let c = ops[k];
        let mut run = 1;
        while k + run < ops.len() && ops[k + run] == c {
            run += 1;
        }
        cigar.push_str(&run.to_string());
        cigar.push(c as char);
        k += run;
    }

    Alignment {
        score,
        cigar,
        x_aligned: String::from_utf8(xa).unwrap_or_default(),
        y_aligned: String::from_utf8(ya).unwrap_or_default(),
        y_start,
        y_end,
    }
}

/// One column of an alignment path produced by [`align_path`]: both sequences
/// contribute (`Both`), only `a` (a gap in `b`), or only `b` (a gap in `a`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Both(usize, usize),
    OnlyA(usize),
    OnlyB(usize),
}

/// Generic affine-gap alignment over index ranges `[0,n) x [0,m)`, matching via
/// `eq(ia, ib)`. Returns the score and the alignment path in left-to-right order.
/// The same Gotoh DP and modes as [`align`]; kept as a separate routine so the
/// tested byte aligner is undisturbed (this powers the general `align(a, b)`
/// builtin over arbitrary Helix values).
pub fn align_path(
    n: usize,
    m: usize,
    mode: Mode,
    sc: Scoring,
    eq: impl Fn(usize, usize) -> bool,
) -> (i64, Vec<Step>) {
    let w = m + 1;
    let size = (n + 1) * w;
    let mut mm = vec![NEG_INF; size];
    let mut xx = vec![NEG_INF; size];
    let mut yy = vec![NEG_INF; size];
    let mut tm = vec![3u8; size];
    let mut tx = vec![3u8; size];
    let mut ty = vec![3u8; size];
    let first_gap = sc.gap_open + sc.gap_extend;
    mm[0] = 0;
    for i in 1..=n {
        let idx = i * w;
        let open = mm[(i - 1) * w] + first_gap;
        let ext = xx[(i - 1) * w] + sc.gap_extend;
        let (v, from) = if open >= ext { (open, 0) } else { (ext, 1) };
        xx[idx] = v;
        tx[idx] = from;
    }
    for j in 1..=m {
        let open = mm[j - 1] + first_gap;
        let ext = yy[j - 1] + sc.gap_extend;
        let (v, from) = if open >= ext { (open, 0) } else { (ext, 2) };
        yy[j] = v;
        ty[j] = from;
    }
    if mode == Mode::Semiglobal {
        for j in 0..=m {
            yy[j] = 0;
            ty[j] = 3;
        }
    }
    for i in 1..=n {
        for j in 1..=m {
            let idx = i * w + j;
            let diag = (i - 1) * w + (j - 1);
            let up = (i - 1) * w + j;
            let left = i * w + (j - 1);
            let sij = if eq(i - 1, j - 1) { sc.match_ } else { sc.mismatch };
            let (best, from) = max3(mm[diag], xx[diag], yy[diag]);
            // Local mode: a fresh start (empty prefix, score 0) may beat continuing a
            // negative-scoring prefix, so clamp the predecessor to >= 0 before adding s.
            let (prefix, pfrom) = if mode == Mode::Local && best < 0 { (0, 3u8) } else { (best, from) };
            mm[idx] = prefix + sij;
            tm[idx] = pfrom;
            if mode == Mode::Local && mm[idx] < 0 {
                mm[idx] = 0;
                tm[idx] = 3;
            }
            let xo = mm[up] + first_gap;
            let xe = xx[up] + sc.gap_extend;
            if xo >= xe {
                xx[idx] = xo;
                tx[idx] = 0;
            } else {
                xx[idx] = xe;
                tx[idx] = 1;
            }
            let yo = mm[left] + first_gap;
            let ye = yy[left] + sc.gap_extend;
            if yo >= ye {
                yy[idx] = yo;
                ty[idx] = 0;
            } else {
                yy[idx] = ye;
                ty[idx] = 2;
            }
        }
    }
    let (mut i, mut j, mut state, score) = match mode {
        Mode::Global => {
            let (sc2, st) = max3(mm[n * w + m], xx[n * w + m], yy[n * w + m]);
            (n, m, st, sc2)
        }
        Mode::Local => {
            let (mut bi, mut bj, mut best) = (0usize, 0usize, 0i64);
            for ii in 0..=n {
                for jj in 0..=m {
                    if mm[ii * w + jj] > best {
                        best = mm[ii * w + jj];
                        bi = ii;
                        bj = jj;
                    }
                }
            }
            (bi, bj, 0u8, best)
        }
        Mode::Semiglobal => {
            let (mut bj, mut bst, mut best) = (0usize, 0u8, NEG_INF);
            for jj in 0..=m {
                if mm[n * w + jj] >= best {
                    best = mm[n * w + jj];
                    bj = jj;
                    bst = 0;
                }
                if xx[n * w + jj] > best {
                    best = xx[n * w + jj];
                    bj = jj;
                    bst = 1;
                }
            }
            (n, bj, bst, best)
        }
    };
    let mut steps: Vec<Step> = Vec::new();
    loop {
        let idx = i * w + j;
        match mode {
            Mode::Local => {
                // stop at an empty (score-0) M cell - the start of the local alignment
                if state == 0 && mm[idx] <= 0 {
                    break;
                }
            }
            Mode::Semiglobal => {
                if i == 0 {
                    break;
                }
            }
            Mode::Global => {
                if i == 0 && j == 0 {
                    break;
                }
            }
        }
        match state {
            0 => {
                steps.push(Step::Both(i - 1, j - 1));
                state = tm[idx];
                i -= 1;
                j -= 1;
                if state == 3 {
                    break; // predecessor is the origin (a fresh local start)
                }
            }
            1 => {
                steps.push(Step::OnlyA(i - 1));
                state = tx[idx];
                i -= 1;
            }
            _ => {
                steps.push(Step::OnlyB(j - 1));
                state = ty[idx];
                j -= 1;
            }
        }
    }
    steps.reverse();
    (score, steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(x: &str, y: &str) -> Alignment {
        align(x.as_bytes(), y.as_bytes(), Mode::Global, Scoring::nucleotide())
    }

    #[test]
    fn global_identical() {
        let a = g("ACGT", "ACGT");
        assert_eq!(a.score, 4);
        assert_eq!(a.cigar, "4=");
        assert_eq!(a.x_aligned, "ACGT");
        assert_eq!(a.y_aligned, "ACGT");
        assert_eq!((a.y_start, a.y_end), (0, 4));
    }

    #[test]
    fn global_one_mismatch() {
        let a = g("ACGT", "AGGT");
        assert_eq!(a.score, 2); // 3 matches - 1 mismatch
        assert_eq!(a.cigar, "1=1X2=");
    }

    #[test]
    fn global_one_insertion_in_query() {
        // Query has an extra C; aligning it against a gap costs gap_open+extend = -6.
        let a = g("ACGT", "AGT");
        assert_eq!(a.score, 1 - 6 + 1 + 1); // A= , C insertion, G=, T=
        assert_eq!(a.cigar, "1=1I2=");
        assert_eq!(a.x_aligned, "ACGT");
        assert_eq!(a.y_aligned, "A-GT");
    }

    #[test]
    fn local_finds_the_conserved_core() {
        // The shared "GATTACA" should align locally at +7 with no gaps.
        let a = align(b"TTGATTACATT", b"CCGATTACAGG", Mode::Local, Scoring::nucleotide());
        assert_eq!(a.score, 7);
        assert_eq!(a.cigar, "7=");
        assert_eq!(a.x_aligned, "GATTACA");
        assert_eq!(a.y_aligned, "GATTACA");
    }

    #[test]
    fn semiglobal_fits_query_into_target() {
        // The whole query GT sits at target positions [2, 4) with free flanking gaps.
        let a = align(b"GT", b"ACGTAC", Mode::Semiglobal, Scoring::nucleotide());
        assert_eq!(a.score, 2);
        assert_eq!(a.cigar, "2=");
        assert_eq!((a.y_start, a.y_end), (2, 4));
    }

    #[test]
    fn affine_prefers_one_long_gap_over_two_short() {
        // The query's extra "CC" aligns to a single 2-base gap in the target
        // (open + 2*extend = -7), beating two separate 1-base gaps (2 * -6 = -12).
        // Extra query bases against gaps are insertions (I).
        let a = g("AACCTT", "AATT");
        assert_eq!(a.cigar, "2=2I2=");
        assert_eq!(a.score, 1 + 1 + (-5 - 2) + 1 + 1); // = -3
        assert_eq!(a.x_aligned, "AACCTT");
        assert_eq!(a.y_aligned, "AA--TT");
    }
}
