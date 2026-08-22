//! `read_gff(path)` — parse a GFF3 (Generic Feature Format) annotation file into a
//! Polars-backed DataFrame, so genomic features flow through the same verbs as any
//! other table:
//!
//! ```text
//! read_gff("genes.gff3").where(type == "gene").group(seqid).count(start)
//! ```
//!
//! Parsing delegates to `noodles-gff`. The nine standard columns are `seqid`,
//! `source`, `type`, `start`, `end`, `score`, `strand`, `phase`, and one string
//! column per attribute tag seen (e.g. `ID`, `Name`, `gene_id`) — a multi-valued
//! attribute is comma-joined. Plain `.gff`/`.gff3` and gzipped `.gff.gz` both work.

use std::collections::HashMap;

use noodles_gff as gff;

use crate::backend::ColData;
use crate::error::{reserve_rows, HelixError};
use crate::bioio::{open_maybe_gzip, widen_f32};

pub fn read_gff(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let inner =
        open_maybe_gzip(path).map_err(|e| err(format!("could not open GFF `{path}`: {e}")))?;
    let mut reader = gff::io::Reader::new(inner);

    let mut seqid: Vec<String> = Vec::new();
    let mut source: Vec<String> = Vec::new();
    let mut ty: Vec<String> = Vec::new();
    let mut start: Vec<i64> = Vec::new();
    let mut end: Vec<i64> = Vec::new();
    let mut score: Vec<Option<f64>> = Vec::new();
    let mut strand: Vec<String> = Vec::new();
    let mut phase: Vec<Option<i64>> = Vec::new();
    // Attribute tags become string columns, in first-seen order (GFF3 attributes
    // are untyped key=value pairs, like a VCF INFO column but always textual).
    let mut attr_keys: Vec<String> = Vec::new();
    let mut attr_rows: Vec<HashMap<String, String>> = Vec::new();

    for result in reader.record_bufs() {
        let rec = result.map_err(|e| err(format!("malformed GFF record in `{path}`: {e}")))?;

        reserve_rows!(
            "GFF records", line, col,
            seqid, source, ty, start, end, score, strand, phase, attr_rows,
        );
        seqid.push(rec.reference_sequence_name().to_string());
        source.push(rec.source().to_string());
        ty.push(rec.ty().to_string());
        start.push(usize::from(rec.start()) as i64);
        end.push(usize::from(rec.end()) as i64);
        score.push(rec.score().map(widen_f32));
        strand.push(strand_str(rec.strand()).to_string());
        phase.push(rec.phase().map(phase_int));

        let mut row: HashMap<String, String> = HashMap::new();
        for (tag, value) in rec.attributes().as_ref() {
            let key = tag.to_string();
            if !attr_keys.contains(&key) {
                attr_keys.push(key.clone());
            }
            row.insert(key, attr_value_str(value));
        }
        attr_rows.push(row);
    }

    let mut columns: Vec<(String, ColData)> = vec![
        ("seqid".into(), ColData::Str(seqid)),
        ("source".into(), ColData::Str(source)),
        ("type".into(), ColData::Str(ty)),
        ("start".into(), ColData::Int(start)),
        ("end".into(), ColData::Int(end)),
        ("score".into(), ColData::Float(score)),
        ("strand".into(), ColData::Str(strand)),
        ("phase".into(), ColData::IntOpt(phase)),
    ];
    for k in &attr_keys {
        let vals: Vec<Option<String>> = attr_rows.iter().map(|r| r.get(k).cloned()).collect();
        columns.push((k.clone(), ColData::StrOpt(vals)));
    }

    crate::backend::build_frame(columns, line, col)
}

/// The single-character strand as GFF writes it.
fn strand_str(s: gff::feature::record::Strand) -> &'static str {
    use gff::feature::record::Strand;
    match s {
        Strand::None => ".",
        Strand::Forward => "+",
        Strand::Reverse => "-",
        Strand::Unknown => "?",
    }
}

/// The CDS reading-frame phase (0, 1, or 2).
fn phase_int(p: gff::feature::record::Phase) -> i64 {
    use gff::feature::record::Phase;
    match p {
        Phase::Zero => 0,
        Phase::One => 1,
        Phase::Two => 2,
    }
}

/// One attribute value as a string; a multi-valued attribute is comma-joined.
fn attr_value_str(v: &gff::feature::record_buf::attributes::field::Value) -> String {
    use gff::feature::record_buf::attributes::field::Value as V;
    match v {
        V::String(s) => s.to_string(),
        V::Array(xs) => xs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(","),
    }
}
