//! `read_vcf(path)` — parse a VCF (Variant Call Format) file into a Polars-backed
//! DataFrame, so genomic variants flow through the same `where`/`select`/`group`/
//! `count` verbs as any other table (ADR 0003's unified model, applied to the
//! flagship bio domain):
//!
//! ```text
//! read_vcf("cohort.vcf").where(gene == "BRCA1").group(consequence).count()
//! ```
//!
//! The eight fixed columns (`chrom`, `pos`, `id`, `ref`, `alt`, `qual`, `filter`)
//! plus every `INFO` key (e.g. `gene`, `consequence`) become DataFrame columns.
//! `.` is read as a null. Both plain `.vcf` and gzipped/BGZF `.vcf.gz` are accepted
//! (the magic bytes are sniffed). Full INFO typing and binary BCF are a later upgrade
//! (delegating to `noodles`).

use std::collections::HashMap;
use std::io::Read;

use flate2::read::MultiGzDecoder;
use polars::prelude::*;

use crate::error::HelixError;

/// Read a possibly-gzipped text file. The gzip magic bytes (`0x1f 0x8b`) trigger a
/// multi-member decoder, which transparently handles both plain gzip and BGZF — the
/// block-gzip `bgzip` produces for `.vcf.gz`, a concatenation of gzip members.
pub(crate) fn read_text_maybe_gzip(path: &str) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut s = String::new();
        MultiGzDecoder::new(&bytes[..]).read_to_string(&mut s)?;
        Ok(s)
    } else {
        String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

pub fn read_vcf(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);
    let text = read_text_maybe_gzip(path)
        .map_err(|e| err(format!("could not open VCF `{path}`: {e}")))?;

    let mut chrom: Vec<String> = Vec::new();
    let mut pos: Vec<i64> = Vec::new();
    let mut id: Vec<Option<String>> = Vec::new();
    let mut ref_: Vec<String> = Vec::new();
    let mut alt: Vec<String> = Vec::new();
    let mut qual: Vec<Option<f64>> = Vec::new();
    let mut filter: Vec<Option<String>> = Vec::new();
    // INFO: first-seen key order + one map per record.
    let mut info_keys: Vec<String> = Vec::new();
    let mut info_rows: Vec<HashMap<String, String>> = Vec::new();

    for raw in text.lines() {
        if raw.starts_with('#') || raw.trim().is_empty() {
            continue; // `##meta` and the `#CHROM` header line (and blanks)
        }
        let f: Vec<&str> = raw.split('\t').collect();
        if f.len() < 8 {
            return Err(err(format!(
                "malformed VCF record (need at least 8 tab-separated columns): {raw}"
            )));
        }
        chrom.push(f[0].to_string());
        pos.push(
            f[1].parse::<i64>()
                .map_err(|_| err(format!("invalid POS in VCF record: `{}`", f[1])))?,
        );
        id.push(dot(f[2]));
        ref_.push(f[3].to_string());
        alt.push(f[4].to_string());
        qual.push(if f[5] == "." { None } else { f[5].parse::<f64>().ok() });
        filter.push(dot(f[6]));

        let mut row = HashMap::new();
        if f[7] != "." {
            for kv in f[7].split(';') {
                if kv.is_empty() {
                    continue;
                }
                let (k, v) = match kv.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => (kv.to_string(), "true".to_string()), // a flag
                };
                if !info_keys.contains(&k) {
                    info_keys.push(k.clone());
                }
                row.insert(k, v);
            }
        }
        info_rows.push(row);
    }

    let mut columns: Vec<Column> = vec![
        Column::new("chrom".into(), chrom),
        Column::new("pos".into(), pos),
        Column::new("id".into(), id),
        Column::new("ref".into(), ref_),
        Column::new("alt".into(), alt),
        Column::new("qual".into(), qual),
        Column::new("filter".into(), filter),
    ];
    // One column per INFO key (string-valued; absent in a record → null).
    for k in &info_keys {
        let vals: Vec<Option<String>> = info_rows.iter().map(|r| r.get(k).cloned()).collect();
        columns.push(Column::new(k.as_str().into(), vals));
    }

    let df = DataFrame::new_infer_height(columns)
        .map_err(|e| err(format!("could not build the VCF table: {e}")))?;
    Ok(crate::backend::polars::from_polars_df(df))
}

fn dot(s: &str) -> Option<String> {
    if s == "." {
        None
    } else {
        Some(s.to_string())
    }
}
