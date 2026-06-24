//! Genomics I/O — the computational-biology flagship.
//!
//! Helix doesn't reimplement bioinformatics; it delegates parsing to best-in-class
//! Rust crates (here, `needletail` for FASTA/FASTQ) and exposes the results
//! through Helix's own value model — the same way DataFrames lean on Polars.

use std::rc::Rc;

use needletail::{parse_fastx_file, FastxReader};

use crate::error::HelixError;
use crate::value::Value;

/// `read_fasta(path)` → an array of sequence records `{id, seq, length}`.
///
/// `seq` is a `Dna` value (uppercased). Ambiguous/soft-masked bases such as `N`
/// or lowercase are preserved on read — the sequence methods (`gc_content`,
/// `complement`, `kmers`, …) handle non-ACGT characters gracefully. Plain `.fa`
/// and gzipped `.fa.gz` are both accepted (needletail sniffs compression).
pub fn read_fasta(path: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let mut reader: Box<dyn FastxReader> = parse_fastx_file(path).map_err(|e| {
        HelixError::new(format!("cannot read FASTA `{}`: {}", path, e), line, col).hint(
            "check the path and that the file is FASTA/FASTQ (optionally gzipped).",
        )
    })?;

    let mut records: Vec<Value> = Vec::new();
    while let Some(rec) = reader.next() {
        let rec = rec.map_err(|e| {
            HelixError::new(format!("malformed record in `{}`: {}", path, e), line, col)
        })?;
        // The header is everything after `>`; the id is its first whitespace token.
        let header = String::from_utf8_lossy(rec.id());
        let id = header.split_whitespace().next().unwrap_or("").to_string();
        let seq: String = rec.seq().iter().map(|b| b.to_ascii_uppercase() as char).collect();
        let length = seq.len() as i64;

        records.push(Value::Record(Rc::new(vec![
            ("id".to_string(), Value::Str(Rc::new(id))),
            ("seq".to_string(), Value::Dna(Rc::new(seq))),
            ("length".to_string(), Value::Int(length)),
        ])));
    }
    Ok(Value::Array(Rc::new(records)))
}

/// `read_fastq(path)` → an array of sequencing-read records `{id, seq, qual, length}`.
///
/// Like [`read_fasta`], but each read also carries its per-base `qual` (Phred
/// quality) string. (A record from a FASTA source, which has no quality line, gets
/// `qual = missing`.) Plain `.fastq` and gzipped `.fastq.gz` are both accepted.
pub fn read_fastq(path: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let mut reader: Box<dyn FastxReader> = parse_fastx_file(path).map_err(|e| {
        HelixError::new(format!("cannot read FASTQ `{}`: {}", path, e), line, col)
            .hint("check the path and that the file is FASTQ (optionally gzipped).")
    })?;

    let mut records: Vec<Value> = Vec::new();
    while let Some(rec) = reader.next() {
        let rec = rec.map_err(|e| {
            HelixError::new(format!("malformed record in `{}`: {}", path, e), line, col)
        })?;
        let header = String::from_utf8_lossy(rec.id());
        let id = header.split_whitespace().next().unwrap_or("").to_string();
        let seq: String = rec.seq().iter().map(|b| b.to_ascii_uppercase() as char).collect();
        let length = seq.len() as i64;
        let qual = match rec.qual() {
            Some(q) => Value::Str(Rc::new(String::from_utf8_lossy(q).into_owned())),
            None => Value::Missing,
        };

        records.push(Value::Record(Rc::new(vec![
            ("id".to_string(), Value::Str(Rc::new(id))),
            ("seq".to_string(), Value::Dna(Rc::new(seq))),
            ("qual".to_string(), qual),
            ("length".to_string(), Value::Int(length)),
        ])));
    }
    Ok(Value::Array(Rc::new(records)))
}
