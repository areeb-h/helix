//! `read_vcf(path)` — parse a VCF (Variant Call Format) file into a Polars-backed
//! DataFrame, so genomic variants flow through the same `where`/`select`/`group`/
//! `count` verbs as any other table (ADR 0003's unified model, applied to the
//! flagship bio domain):
//!
//! ```text
//! read_vcf("cohort.vcf").where(gene == "BRCA1").group(consequence).count()
//! ```
//!
//! Parsing delegates to `noodles-vcf` (the bio flagship stands on the Rust bio
//! ecosystem the way it stands on Polars), so the header drives the schema:
//!
//! - The eight fixed columns `chrom`, `pos`, `id`, `ref`, `alt`, `qual`, `filter`.
//! - One column per `INFO` field **typed from its header declaration** — an
//!   `Integer` field becomes an `i64` column, a `Float` an `f64`, a `Flag` a
//!   `bool`, a `String`/`Character` a string. So `where(af > 0.001)` is a numeric
//!   comparison, not a string one. Multi-valued INFO fields (`Number != 1`) are
//!   rendered as comma-joined strings to stay queryable without list columns.
//! - INFO keys present in the data but undeclared in the header still appear, as
//!   string columns (a tolerance for slightly non-conformant files).
//!
//! Plain `.vcf` and gzipped/BGZF `.vcf.gz` are both accepted (the magic bytes are
//! sniffed; BGZF is a concatenation of gzip members, which the multi-member decoder
//! reads transparently). `read_bcf` parses the binary BCF form through the same
//! record model and column-building core — only the reader construction differs.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};

use flate2::read::MultiGzDecoder;
use noodles_bcf as bcf;
use noodles_vcf::{self as vcf, header::record::value::map::info::{Number, Type as InfoType}};

use crate::backend::ColData;
use crate::error::HelixError;

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

/// The Polars dtype a given INFO key maps to, decided once from the header.
#[derive(Clone, Copy)]
enum ColKind {
    Int,
    Float,
    Bool,
    Str,
}

/// One INFO cell's value, normalized out of noodles' typed field value.
enum Cell {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// `read_vcf(path)` — parse a (optionally gzip/BGZF-compressed) text VCF.
pub fn read_vcf(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let inner =
        open_maybe_gzip(path).map_err(|e| err(format!("could not open VCF `{path}`: {e}")))?;
    let mut reader = vcf::io::Reader::new(inner);
    let header = reader
        .read_header()
        .map_err(|e| err(format!("could not read VCF header in `{path}`: {e}")))?;
    let records = reader.record_bufs(&header);
    build_variant_frame(&header, records, "VCF", path, line, col)
}

/// `read_bcf(path)` — parse a BCF file (the binary, BGZF-framed form of VCF). BCF
/// carries the same header and the same noodles `RecordBuf` records as VCF, so the
/// schema derivation and column building below are shared verbatim with `read_vcf`;
/// only the reader construction differs. `bcf::io::Reader::new` decodes the BGZF
/// framing internally, so a plain `File` is handed to it.
pub fn read_bcf(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let file =
        std::fs::File::open(path).map_err(|e| err(format!("could not open BCF `{path}`: {e}")))?;
    let mut reader = bcf::io::Reader::new(file);
    let header = reader
        .read_header()
        .map_err(|e| err(format!("could not read BCF header in `{path}`: {e}")))?;
    let records = reader.record_bufs(&header);
    build_variant_frame(&header, records, "BCF", path, line, col)
}

/// Build a variant DataFrame from a parsed header and a stream of records, shared
/// by [`read_vcf`] and [`read_bcf`]. `format` (e.g. `"VCF"`/`"BCF"`) only labels
/// the malformed-record error. The eight fixed columns plus one header-typed column
/// per INFO field are emitted through the backend seam (ADR 0012).
fn build_variant_frame<I>(
    header: &vcf::Header,
    records: I,
    format: &str,
    path: &str,
    line: usize,
    col: usize,
) -> Result<crate::backend::Df, HelixError>
where
    I: Iterator<Item = std::io::Result<vcf::variant::RecordBuf>>,
{
    let err = |msg: String| HelixError::new(msg, line, col);

    // The INFO schema comes from the header: a stable, declared-order key list and
    // the column kind each maps to. A scalar (`Number=1`) Integer/Float/String gets
    // a typed column; a Flag a bool; anything multi-valued a (joined) string.
    let mut info_keys: Vec<String> = Vec::new();
    let mut kind: HashMap<String, ColKind> = HashMap::new();
    for (key, def) in header.infos() {
        let k = match (def.ty(), def.number()) {
            (InfoType::Flag, _) => ColKind::Bool,
            (InfoType::Integer, Number::Count(1)) => ColKind::Int,
            (InfoType::Float, Number::Count(1)) => ColKind::Float,
            _ => ColKind::Str,
        };
        info_keys.push(key.clone());
        kind.insert(key.clone(), k);
    }

    let mut chrom: Vec<String> = Vec::new();
    let mut pos: Vec<Option<i64>> = Vec::new();
    let mut id: Vec<Option<String>> = Vec::new();
    let mut ref_: Vec<String> = Vec::new();
    let mut alt: Vec<String> = Vec::new();
    let mut qual: Vec<Option<f64>> = Vec::new();
    let mut filter: Vec<Option<String>> = Vec::new();
    let mut info_rows: Vec<HashMap<String, Cell>> = Vec::new();

    for result in records {
        let rec =
            result.map_err(|e| err(format!("malformed {format} record in `{path}`: {e}")))?;

        chrom.push(rec.reference_sequence_name().to_string());
        pos.push(rec.variant_start().map(|p| usize::from(p) as i64));
        id.push(join_opt(rec.ids().as_ref().iter().map(|s| s.to_string()), ';'));
        ref_.push(rec.reference_bases().to_string());
        alt.push(
            rec.alternate_bases()
                .as_ref()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        qual.push(rec.quality_score().map(widen_f32));
        filter.push(join_opt(rec.filters().as_ref().iter().map(|s| s.to_string()), ';'));

        let info = rec.info();
        let mut row: HashMap<String, Cell> = HashMap::new();
        for (key, value) in info.keys().zip(info.values()) {
            let cell = match value {
                Some(v) => value_to_cell(v),
                None => Cell::Bool(true), // a Flag present without a value
            };
            if !kind.contains_key(key) {
                // Undeclared in the header — tolerate it as a string column.
                info_keys.push(key.to_string());
                kind.insert(key.to_string(), ColKind::Str);
            }
            row.insert(key.to_string(), cell);
        }
        info_rows.push(row);
    }

    // A header `Number=1` field whose records actually carry multiple values (or a
    // type the header misdeclares) would be silently nulled by the typed column.
    // Downgrade any such field to a string column so no data is dropped.
    for k in &info_keys {
        let mismatched = match kind[k] {
            ColKind::Int => info_rows
                .iter()
                .any(|r| matches!(r.get(k), Some(c) if !matches!(c, Cell::Int(_)))),
            ColKind::Float => info_rows
                .iter()
                .any(|r| matches!(r.get(k), Some(Cell::Str(_)) | Some(Cell::Bool(_)))),
            _ => false,
        };
        if mismatched {
            kind.insert(k.clone(), ColKind::Str);
        }
    }

    let fixed = ["chrom", "pos", "id", "ref", "alt", "qual", "filter"];
    let mut columns: Vec<(String, ColData)> = vec![
        ("chrom".into(), ColData::Str(chrom)),
        ("pos".into(), ColData::IntOpt(pos)),
        ("id".into(), ColData::StrOpt(id)),
        ("ref".into(), ColData::Str(ref_)),
        ("alt".into(), ColData::Str(alt)),
        ("qual".into(), ColData::Float(qual)),
        ("filter".into(), ColData::StrOpt(filter)),
    ];
    for k in &info_keys {
        // An INFO key colliding with a fixed column (e.g. an INFO field literally
        // named `pos`) is prefixed rather than producing a duplicate column.
        let cname = if fixed.contains(&k.as_str()) { format!("info_{k}") } else { k.clone() };
        columns.push((cname, build_info_column(k, kind[k], &info_rows)));
    }

    crate::backend::build_frame(columns, line, col)
}

/// Widen an `f32` (how noodles parses a text-VCF `Float`/`QUAL`) to `f64` through
/// its shortest round-trip decimal, *not* a raw `as f64` cast. A raw cast exposes
/// the binary `f32` error — `0.001_f32 as f64` is 0.00100000004…, so `af > 0.001`
/// would spuriously match a `0.001` row. Round-tripping through the shortest decimal
/// recovers the value the VCF author actually wrote, so comparisons behave as a
/// scientist expects. Shared with the other genomics readers (GFF score).
pub(crate) fn widen_f32(f: f32) -> f64 {
    f.to_string().parse::<f64>().unwrap_or(f as f64)
}

/// Join an iterator of strings with `sep`, yielding `None` when empty (the VCF `.`
/// missing marker collapses to a null cell).
fn join_opt(parts: impl Iterator<Item = String>, sep: char) -> Option<String> {
    let joined: Vec<String> = parts.collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join(&sep.to_string()))
    }
}

/// Normalize one noodles INFO field value into a [`Cell`]. Multi-valued (`Array`)
/// fields collapse to a comma-joined string so the column stays a plain string.
fn value_to_cell(v: &vcf::variant::record_buf::info::field::Value) -> Cell {
    use vcf::variant::record_buf::info::field::Value as V;
    match v {
        V::Integer(i) => Cell::Int(*i as i64),
        V::Float(f) => Cell::Float(widen_f32(*f)),
        V::Flag => Cell::Bool(true),
        V::Character(c) => Cell::Str(c.to_string()),
        V::String(s) => Cell::Str(s.clone()),
        V::Array(a) => Cell::Str(format_array(a)),
    }
}

/// Render a multi-valued INFO array as the comma-joined string VCF uses on the
/// wire (`AF=0.01,0.02`), a missing element as `.`. Keeps the column a plain,
/// queryable string rather than a Polars list column.
fn format_array(a: &vcf::variant::record_buf::info::field::value::Array) -> String {
    use vcf::variant::record_buf::info::field::value::Array as A;
    fn join<T: ToString>(xs: &[Option<T>]) -> String {
        xs.iter()
            .map(|o| o.as_ref().map_or_else(|| ".".to_string(), |v| v.to_string()))
            .collect::<Vec<_>>()
            .join(",")
    }
    match a {
        A::Integer(xs) => join(xs),
        A::Float(xs) => xs
            .iter()
            .map(|o| o.map_or_else(|| ".".to_string(), |f| widen_f32(f).to_string()))
            .collect::<Vec<_>>()
            .join(","),
        A::Character(xs) => join(xs),
        A::String(xs) => join(xs),
    }
}

/// Build one typed INFO column by pulling each record's cell (or a null/`false`).
fn build_info_column(key: &str, kind: ColKind, rows: &[HashMap<String, Cell>]) -> ColData {
    match kind {
        ColKind::Int => ColData::IntOpt(
            rows.iter()
                .map(|r| match r.get(key) {
                    Some(Cell::Int(i)) => Some(*i),
                    _ => None,
                })
                .collect(),
        ),
        ColKind::Float => ColData::Float(
            rows.iter()
                .map(|r| match r.get(key) {
                    Some(Cell::Float(f)) => Some(*f),
                    Some(Cell::Int(i)) => Some(*i as f64),
                    _ => None,
                })
                .collect(),
        ),
        // Flag semantics: present → true, absent → false (never null).
        ColKind::Bool => ColData::Bool(
            rows.iter().map(|r| matches!(r.get(key), Some(Cell::Bool(true)))).collect(),
        ),
        ColKind::Str => ColData::StrOpt(
            rows.iter()
                .map(|r| match r.get(key) {
                    Some(Cell::Str(s)) => Some(s.clone()),
                    Some(Cell::Int(i)) => Some(i.to_string()),
                    Some(Cell::Float(f)) => Some(f.to_string()),
                    Some(Cell::Bool(b)) => Some(b.to_string()),
                    None => None,
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerate `examples/data/variants.bcf` from the text `variants.vcf` using
    /// noodles' BCF writer (no external `bcftools` needed). Ignored by default; run
    /// on demand with `cargo test -- --ignored generate_bcf_fixture` whenever the
    /// VCF fixture changes. Documents how the committed binary fixture was produced.
    #[test]
    #[ignore]
    fn generate_bcf_fixture() {
        use vcf::variant::io::Write as _;

        let inner = open_maybe_gzip("examples/data/variants.vcf").unwrap();
        let mut reader = vcf::io::Reader::new(inner);
        let header = reader.read_header().unwrap();

        let out = std::fs::File::create("examples/data/variants.bcf").unwrap();
        let mut writer = bcf::io::Writer::new(out);
        writer.write_variant_header(&header).unwrap();
        for result in reader.record_bufs(&header) {
            let record = result.unwrap();
            writer.write_variant_record(&header, &record).unwrap();
        }
        writer.try_finish().unwrap();
    }
}
