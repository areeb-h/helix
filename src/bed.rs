//! `read_bed(path)` — parse a BED (Browser Extensible Data) interval file into a
//! Polars-backed DataFrame:
//!
//! ```text
//! read_bed("peaks.bed").where(end - start > 1000).count()
//! ```
//!
//! BED is a headerless, whitespace-separated format with three mandatory columns
//! and optional ones, so — unlike VCF/GFF, where the header and field typing earn a
//! spec parser — it is read directly. The first three columns become `chrom`,
//! `start`, `end` (coordinates are BED's native 0-based, half-open, kept as written);
//! `name`, `score`, `strand` columns are added when the file carries them. `track`/
//! `browser`/comment lines are skipped. Plain `.bed` and gzipped `.bed.gz` both work.

use std::io::BufRead;

use polars::prelude::*;

use crate::error::HelixError;
use crate::vcf::open_maybe_gzip;

pub fn read_bed(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let reader =
        open_maybe_gzip(path).map_err(|e| err(format!("could not open BED `{path}`: {e}")))?;

    let mut chrom: Vec<String> = Vec::new();
    let mut start: Vec<i64> = Vec::new();
    let mut end: Vec<i64> = Vec::new();
    let mut name: Vec<Option<String>> = Vec::new();
    let mut score: Vec<Option<i64>> = Vec::new();
    let mut strand: Vec<Option<String>> = Vec::new();
    let mut max_cols = 3usize; // widest standard-field row seen (caps at 6)

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
        name.push(f.get(3).filter(|s| **s != ".").map(|s| s.to_string()));
        score.push(f.get(4).and_then(|s| s.parse::<i64>().ok()));
        strand.push(f.get(5).filter(|s| **s != ".").map(|s| s.to_string()));
        max_cols = max_cols.max(f.len().min(6));
    }

    let mut columns: Vec<Column> = vec![
        Column::new("chrom".into(), chrom),
        Column::new("start".into(), start),
        Column::new("end".into(), end),
    ];
    // Only surface the optional columns the file actually carries.
    if max_cols >= 4 {
        columns.push(Column::new("name".into(), name));
    }
    if max_cols >= 5 {
        columns.push(Column::new("score".into(), score));
    }
    if max_cols >= 6 {
        columns.push(Column::new("strand".into(), strand));
    }

    let df = DataFrame::new_infer_height(columns)
        .map_err(|e| err(format!("could not build the BED table: {e}")))?;
    Ok(crate::backend::polars::from_polars_df(df))
}
