//! `read_bed(path)` — parse a BED (Browser Extensible Data) interval file into a
//! DataFrame:
//!
//! ```text
//! read_bed("peaks.bed").where(end - start > 1000).count()
//! ```
//!
//! BED is a headerless, whitespace-separated format with three mandatory columns
//! and up to nine optional ones, so — unlike VCF/GFF, where the header and field
//! typing earn a spec parser — it is read directly. The first three columns become
//! `chrom`, `start`, `end` (coordinates are BED's native 0-based, half-open, kept as
//! written); the standard optional columns (`name`, `score`, `strand`, `thickStart`,
//! `thickEnd`, `itemRgb`, `blockCount`, `blockSizes`, `blockStarts`) are added when
//! the file carries them — a full BED3–BED12. `track`/`browser`/comment lines are
//! skipped. Plain `.bed` and gzipped `.bed.gz` both work.

use std::io::BufRead;

use crate::backend::ColData;
use crate::error::HelixError;
use crate::vcf::open_maybe_gzip;

pub fn read_bed(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let reader =
        open_maybe_gzip(path).map_err(|e| err(format!("could not open BED `{path}`: {e}")))?;

    let mut chrom: Vec<String> = Vec::new();
    let mut start: Vec<i64> = Vec::new();
    let mut end: Vec<i64> = Vec::new();
    // Optional standard columns 4–12.
    let mut name: Vec<Option<String>> = Vec::new();
    let mut score: Vec<Option<i64>> = Vec::new();
    let mut strand: Vec<Option<String>> = Vec::new();
    let mut thick_start: Vec<Option<i64>> = Vec::new();
    let mut thick_end: Vec<Option<i64>> = Vec::new();
    let mut item_rgb: Vec<Option<String>> = Vec::new();
    let mut block_count: Vec<Option<i64>> = Vec::new();
    let mut block_sizes: Vec<Option<String>> = Vec::new();
    let mut block_starts: Vec<Option<String>> = Vec::new();
    let mut max_cols = 3usize; // widest standard-field row seen (caps at 12)

    // `.` is BED's missing marker; a non-`.` field maps to its string/int value.
    let opt_str = |f: &[&str], i: usize| f.get(i).filter(|s| **s != ".").map(|s| s.to_string());
    let opt_int = |f: &[&str], i: usize| {
        f.get(i).filter(|s| **s != ".").and_then(|s| s.parse::<i64>().ok())
    };

    for (i, raw) in reader.lines().enumerate() {
        let raw = raw.map_err(|e| err(format!("could not read BED `{path}`: {e}")))?;
        let l = raw.trim();
        if l.is_empty()
            || l.starts_with('#')
            || l.starts_with("track")
            || l.starts_with("browser")
        {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 {
            return Err(err(format!(
                "malformed BED record on line {} (need at least chrom/start/end): {l}",
                i + 1
            )));
        }
        chrom.push(f[0].to_string());
        start.push(
            f[1].parse::<i64>().map_err(|_| err(format!("invalid BED start: `{}`", f[1])))?,
        );
        end.push(f[2].parse::<i64>().map_err(|_| err(format!("invalid BED end: `{}`", f[2])))?);
        name.push(opt_str(&f, 3));
        score.push(opt_int(&f, 4));
        strand.push(opt_str(&f, 5));
        thick_start.push(opt_int(&f, 6));
        thick_end.push(opt_int(&f, 7));
        item_rgb.push(opt_str(&f, 8));
        block_count.push(opt_int(&f, 9));
        block_sizes.push(opt_str(&f, 10));
        block_starts.push(opt_str(&f, 11));
        max_cols = max_cols.max(f.len().min(12));
    }

    let mut columns: Vec<(String, ColData)> = vec![
        ("chrom".into(), ColData::Str(chrom)),
        ("start".into(), ColData::Int(start)),
        ("end".into(), ColData::Int(end)),
    ];
    // Surface each optional column only when the file actually carries it.
    let optional: [(usize, &str, ColData); 9] = [
        (4, "name", ColData::StrOpt(name)),
        (5, "score", ColData::IntOpt(score)),
        (6, "strand", ColData::StrOpt(strand)),
        (7, "thickStart", ColData::IntOpt(thick_start)),
        (8, "thickEnd", ColData::IntOpt(thick_end)),
        (9, "itemRgb", ColData::StrOpt(item_rgb)),
        (10, "blockCount", ColData::IntOpt(block_count)),
        (11, "blockSizes", ColData::StrOpt(block_sizes)),
        (12, "blockStarts", ColData::StrOpt(block_starts)),
    ];
    for (need, cname, data) in optional {
        if max_cols >= need {
            columns.push((cname.into(), data));
        }
    }

    crate::backend::build_frame(columns, line, col)
}
