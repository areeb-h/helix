//! Shared bio file-format plumbing — the pieces every genomics reader leans on
//! that carry NO heavy dependency of their own. Hoisted out of `vcf.rs` (ADR 0032)
//! so `bed.rs` (a hand-rolled parser, no noodles) and `gff.rs` don't need the
//! noodles-backed module compiled just to open a gzip or widen a float, and so the
//! `bio` feature can gate the noodles/needletail modules cleanly.

use std::io::{BufRead, BufReader};

use flate2::read::MultiGzDecoder;

/// Open `path` as a buffered byte stream, transparently decompressing gzip/BGZF.
/// BGZF (what `bgzip` produces for `.vcf.gz`) is a concatenation of gzip members,
/// which `MultiGzDecoder` handles like any multi-member gzip. Shared by the
/// genomics readers.
pub(crate) fn open_maybe_gzip(path: &str) -> std::io::Result<Box<dyn BufRead>> {
    let mut file = BufReader::new(std::fs::File::open(path)?);
    let is_gzip = {
        let head = file.fill_buf()?;
        head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b
    };
    if is_gzip {
        Ok(Box::new(BufReader::new(MultiGzDecoder::new(file))))
    } else {
        Ok(Box::new(file))
    }
}

/// Widen an `f32` (how noodles parses a text-VCF `Float`/`QUAL`) to `f64` through
/// its shortest round-trip decimal, *not* a raw `as f64` cast. A raw cast exposes
/// the binary `f32` error — `0.001_f32 as f64` is 0.00100000004…, so `af > 0.001`
/// would spuriously match a `0.001` row. Round-tripping through the shortest decimal
/// recovers the value the VCF author actually wrote, so comparisons behave as a
/// scientist expects. Shared with the other genomics readers (GFF score).
#[cfg_attr(not(feature = "bio"), allow(dead_code))]
pub(crate) fn widen_f32(f: f32) -> f64 {
    f.to_string().parse::<f64>().unwrap_or(f as f64)
}

/// The error a genomics reader answers with in a build without the `bio` feature —
/// the ADR 0032 gate-the-body shape: the builtin still exists, type-checks, and
/// describes itself; running it says what to rebuild with.
#[cfg(not(feature = "bio"))]
pub fn no_bio(line: usize, col: usize) -> crate::error::HelixError {
    crate::error::HelixError::new("this build has no genomics-reader support", line, col)
        .hint("build without `--no-default-features`, or with `--features bio`.")
}
