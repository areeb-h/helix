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

/// Validate + normalize a sequence read from a file into the `Dna` invariant —
/// the SAME rule `dna()` applies (uppercase; `A C G T N` plus the IUPAC
/// ambiguity codes `R Y S W K M B D H V`).
///
/// The readers used to uppercase WITHOUT validating, minting `Dna` values that
/// `dna()` itself would reject — and the sequence methods, which are written
/// against this invariant, then answered with plausible nonsense instead of
/// erroring: a `>s1 / ATGCXXZZ!!` record gave `gc_content() = 0.2` (counted over
/// the garbage) and `kmers(3) = ["ATG", "TGC"]` (2 k-mers where a 10-base
/// sequence must yield 8, the rest silently dropped), and the value could not be
/// round-tripped through `dna()`. A scientist reading a corrupt FASTA got a
/// believable GC number and no warning. Enforcing the invariant at the boundary
/// is what makes every downstream method's assumption true.
fn dna_from_record(
    seq: &[u8],
    id: &str,
    path: &str,
    line: usize,
    col: usize,
) -> Result<String, HelixError> {
    let mut out = String::with_capacity(seq.len());
    for (i, b) in seq.iter().enumerate() {
        let up = (*b as char).to_ascii_uppercase();
        if crate::interp::is_iupac_dna(up) {
            out.push(up);
        } else {
            return Err(HelixError::new(
                format!(
                    "`{}` is not a valid DNA base (record `{}`, position {})",
                    (*b as char).escape_default(),
                    id,
                    i
                ),
                line,
                col,
            )
            .hint(format!(
                "DNA may contain A, C, G, T, N, or an IUPAC ambiguity code (R Y S W K M B D H V). \
                 Check `{}` in {} — a protein FASTA or a corrupt record reads this way.",
                id, path
            )));
        }
    }
    Ok(out)
}

/// `read_fasta(path)` → an array of sequence records `{id, seq, length}`.
///
/// `seq` is a `Dna` value, normalized and VALIDATED exactly as `dna()` does:
/// uppercased, and restricted to `A C G T N` plus the IUPAC ambiguity codes
/// (`R Y S W K M B D H V`). Lowercase soft-masking and ambiguity codes are
/// therefore read fine; anything else — a protein FASTA, a corrupt record — is a
/// clean error naming the record and position, NOT a `Dna` value that lies to
/// every method downstream (see [`dna_from_record`]). Plain `.fa` and gzipped
/// `.fa.gz` are both accepted (needletail sniffs compression).
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
        let seq: String = dna_from_record(&rec.seq(), &id, path, line, col)?;
        let length = seq.len() as i64;

        crate::error::try_push(
            &mut records,
            Value::Record(Rc::new(vec![
                (k_id, Value::Str(Rc::new(id))),
                (k_seq, Value::Dna(Rc::new(seq))),
                (k_length, Value::Int(length)),
            ])),
            "FASTA records",
            line,
            col,
        )?;
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
        let seq: String = dna_from_record(&rec.seq(), &id, path, line, col)?;
        let length = seq.len() as i64;
        let qual = match rec.qual() {
            Some(q) => Value::Str(Rc::new(String::from_utf8_lossy(q).into_owned())),
            None => Value::Missing,
        };

        crate::error::try_push(
            &mut records,
            Value::Record(Rc::new(vec![
                (k_id, Value::Str(Rc::new(id))),
                (k_seq, Value::Dna(Rc::new(seq))),
                (k_qual, qual),
                (k_length, Value::Int(length)),
            ])),
            "FASTQ records",
            line,
            col,
        )?;
    }
    Ok(Value::array(records))
}
