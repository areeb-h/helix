//! Genomics I/O — the computational-biology flagship.
//!
//! Helix doesn't reimplement bioinformatics; it delegates parsing to best-in-class
//! Rust crates (here, `needletail` for FASTA/FASTQ) and exposes the results
//! through Helix's own value model — the same way DataFrames lean on Polars.

use std::rc::Rc;

use needletail::{parse_fastx_file, FastxReader};

use crate::error::HelixError;
use crate::symbol::Symbol;
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

    // Intern the field names once, not per record (millions of reads).
    let (k_id, k_seq, k_length) =
        (Symbol::intern("id"), Symbol::intern("seq"), Symbol::intern("length"));
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
            (k_id, Value::Str(Rc::new(id))),
            (k_seq, Value::Dna(Rc::new(seq))),
            (k_length, Value::Int(length)),
        ])));
    }
    Ok(Value::array(records))
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

    // Intern the field names once, not per record (millions of reads).
    let (k_id, k_seq, k_qual, k_length) = (
        Symbol::intern("id"),
        Symbol::intern("seq"),
        Symbol::intern("qual"),
        Symbol::intern("length"),
    );
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
            (k_id, Value::Str(Rc::new(id))),
            (k_seq, Value::Dna(Rc::new(seq))),
            (k_qual, qual),
            (k_length, Value::Int(length)),
        ])));
    }
    Ok(Value::array(records))
}
