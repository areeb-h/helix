//! `read_sam(path)` / `read_bam(path)` — parse sequence alignments (SAM/BAM) into
//! a Polars-backed DataFrame, so aligned reads flow through the same
//! `where`/`select`/`group`/`count` verbs as any other table (ADR 0003's unified
//! model, applied to the alignment domain):
//!
//! ```text
//! read_bam("sample.bam").where(@mapq > 30).group(@ref).count(@pos)
//! ```
//!
//! Parsing delegates to `noodles-sam`/`noodles-bam`. BAM is the binary, BGZF-framed
//! form of SAM; noodles yields the same `sam::alignment::RecordBuf` for both, so the
//! column-building core ([`build_alignment_frame`]) is shared verbatim — only the
//! reader construction differs (`bam::io::Reader::new` decodes the BGZF framing
//! internally, so a plain `File` is handed to it).
//!
//! Each record becomes a row with the eleven mandatory SAM fields as columns:
//! `name`, `flag`, `ref`, `pos`, `mapq`, `cigar`, `rnext`, `pnext`, `tlen`, `seq`,
//! `qual`. The reference fields (`ref`/`rnext`) are resolved from the header's
//! reference-sequence dictionary to their names (a null for an unmapped read); `seq`
//! is the read bases and `qual` the Phred quality string (both null when absent).

use std::io;

use noodles_bam as bam;
use noodles_core::Region;
use noodles_sam::{
    self as sam,
    alignment::{record::cigar::op::Kind, record_buf::Cigar, RecordBuf},
};

use crate::backend::ColData;
use crate::error::{reserve_rows, HelixError};

/// `read_sam(path)` — parse a (optionally gzip-compressed) text SAM file.
pub fn read_sam(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let inner = crate::bioio::open_maybe_gzip(path)
        .map_err(|e| err(format!("could not open SAM `{path}`: {e}")))?;
    let mut reader = sam::io::Reader::new(inner);
    let header = reader
        .read_header()
        .map_err(|e| err(format!("could not read SAM header in `{path}`: {e}")))?;
    let records = reader.record_bufs(&header);
    build_alignment_frame(&header, records, "SAM", path, line, col)
}

/// `read_bam(path)` — parse a BAM file (the binary, BGZF-framed form of SAM). BAM
/// carries the same header and the same `RecordBuf` records as SAM, so the column
/// building below is shared verbatim with `read_sam`.
pub fn read_bam(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let file =
        std::fs::File::open(path).map_err(|e| err(format!("could not open BAM `{path}`: {e}")))?;
    let mut reader = bam::io::Reader::new(file);
    let header = reader
        .read_header()
        .map_err(|e| err(format!("could not read BAM header in `{path}`: {e}")))?;
    let records = reader.record_bufs(&header);
    build_alignment_frame(&header, records, "BAM", path, line, col)
}

/// `read_bam(path, region)` — read only the alignments intersecting `region` (e.g.
/// `"chr1:1000-2000"`) from a coordinate-sorted, BAI-indexed BAM, seeking via the
/// `.bai` index instead of scanning the whole file (the local-first capability).
/// Requires a `.bai` sibling. The indexed reader yields lazy records, materialized
/// into the same `RecordBuf`s the scan path uses, so the resulting DataFrame is
/// identical to a full read filtered to the region — only the untouched alignments
/// are never decoded.
pub fn read_bam_region(
    path: &str,
    region: &str,
    line: usize,
    col: usize,
) -> Result<crate::backend::Df, HelixError> {
    let err = |msg: String| HelixError::new(msg, line, col);

    let mut reader = bam::io::indexed_reader::Builder::default()
        .build_from_path(path)
        .map_err(|e| {
            err(format!(
                "could not open indexed BAM `{path}`: {e} (an indexed region query \
                 needs a coordinate-sorted BAM with a `.bai` index alongside it)"
            ))
        })?;
    let header = reader
        .read_header()
        .map_err(|e| err(format!("could not read BAM header in `{path}`: {e}")))?;
    let region: Region =
        region.parse().map_err(|e| err(format!("invalid region `{region}`: {e}")))?;
    let query = reader
        .query(&header, &region)
        .map_err(|e| err(format!("could not query region in `{path}`: {e}")))?;
    // The indexed reader yields lazy records; materialize each into the `RecordBuf`
    // the shared column-building core expects (the same type the scan path produces).
    let records = query.records().map(|r| {
        r.and_then(|rec| RecordBuf::try_from_alignment_record(&header, &rec))
    });
    build_alignment_frame(&header, records, "BAM", path, line, col)
}

/// Build an alignment DataFrame from a parsed header and a stream of records, shared
/// by [`read_sam`] and [`read_bam`]. `format` (e.g. `"SAM"`/`"BAM"`) only labels the
/// malformed-record error. The eleven mandatory SAM columns are emitted through the
/// backend seam (ADR 0012).
fn build_alignment_frame<I>(
    header: &sam::Header,
    records: I,
    format: &str,
    path: &str,
    line: usize,
    col: usize,
) -> Result<crate::backend::Df, HelixError>
where
    I: Iterator<Item = io::Result<RecordBuf>>,
{
    let err = |msg: String| HelixError::new(msg, line, col);

    let mut name: Vec<Option<String>> = Vec::new();
    let mut flag: Vec<i64> = Vec::new();
    let mut ref_: Vec<Option<String>> = Vec::new();
    let mut pos: Vec<Option<i64>> = Vec::new();
    let mut mapq: Vec<Option<i64>> = Vec::new();
    let mut cigar: Vec<Option<String>> = Vec::new();
    let mut rnext: Vec<Option<String>> = Vec::new();
    let mut pnext: Vec<Option<i64>> = Vec::new();
    let mut tlen: Vec<i64> = Vec::new();
    let mut seq: Vec<Option<String>> = Vec::new();
    let mut qual: Vec<Option<String>> = Vec::new();

    for result in records {
        let rec =
            result.map_err(|e| err(format!("malformed {format} record in `{path}`: {e}")))?;

        reserve_rows!(
            "SAM/BAM records", line, col,
            name, flag, ref_, pos, mapq, cigar, rnext, pnext, tlen, seq, qual,
        );
        name.push(rec.name().map(|n| n.to_string()));
        flag.push(u16::from(rec.flags()) as i64);
        ref_.push(ref_name(header, rec.reference_sequence_id()));
        pos.push(rec.alignment_start().map(|p| usize::from(p) as i64));
        mapq.push(rec.mapping_quality().map(|m| u8::from(m) as i64));
        cigar.push(render_cigar(rec.cigar()));
        rnext.push(ref_name(header, rec.mate_reference_sequence_id()));
        pnext.push(rec.mate_alignment_start().map(|p| usize::from(p) as i64));
        tlen.push(rec.template_length() as i64);
        seq.push(render_sequence(rec.sequence()));
        qual.push(render_quality(rec.quality_scores()));
    }

    let columns: Vec<(String, ColData)> = vec![
        ("name".into(), ColData::StrOpt(name)),
        ("flag".into(), ColData::Int(flag)),
        ("ref".into(), ColData::StrOpt(ref_)),
        ("pos".into(), ColData::IntOpt(pos)),
        ("mapq".into(), ColData::IntOpt(mapq)),
        ("cigar".into(), ColData::StrOpt(cigar)),
        ("rnext".into(), ColData::StrOpt(rnext)),
        ("pnext".into(), ColData::IntOpt(pnext)),
        ("tlen".into(), ColData::Int(tlen)),
        ("seq".into(), ColData::StrOpt(seq)),
        ("qual".into(), ColData::StrOpt(qual)),
    ];
    crate::backend::build_frame(columns, line, col)
}

/// Resolve a 0-based reference-sequence id into its name via the header's reference
/// dictionary; `None` (an unmapped read or absent mate) becomes a null cell.
fn ref_name(header: &sam::Header, id: Option<usize>) -> Option<String> {
    let id = id?;
    header.reference_sequences().get_index(id).map(|(n, _)| n.to_string())
}

/// Render a CIGAR as its SAM string (e.g. `36M4D8S`), or `None` when empty (the SAM
/// `*` no-CIGAR marker). Kinds are mapped to their SAM operation characters; the
/// record-buffer `Op` carries no `Display`, so the mapping is explicit here.
fn render_cigar(cigar: &Cigar) -> Option<String> {
    use std::fmt::Write as _;
    let ops = cigar.as_ref();
    if ops.is_empty() {
        return None;
    }
    let mut s = String::new();
    for &op in ops {
        let ch = match op.kind() {
            Kind::Match => 'M',
            Kind::Insertion => 'I',
            Kind::Deletion => 'D',
            Kind::Skip => 'N',
            Kind::SoftClip => 'S',
            Kind::HardClip => 'H',
            Kind::Pad => 'P',
            Kind::SequenceMatch => '=',
            Kind::SequenceMismatch => 'X',
        };
        let _ = write!(s, "{}{}", op.len(), ch);
    }
    Some(s)
}

/// Render the read bases as a string, or `None` for an absent sequence (SAM `*`).
fn render_sequence(seq: &sam::alignment::record_buf::Sequence) -> Option<String> {
    if seq.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(seq.as_ref()).into_owned())
    }
}

/// Render base qualities as the SAM Phred+33 ASCII string, or `None` when absent
/// (SAM `*`). SAM Phred scores are `0..=93`, so each maps to a printable character
/// `!`..`~`. A **BAM** stores scores as raw bytes and noodles does not range-check
/// them, so a malformed/garbage file can carry any byte `0..=255`; the score is
/// clamped to the valid `0..=93` before the `+ 33`, which keeps the sum in `u8`
/// range (max `126`) instead of overflowing — `s + 33` for `s >= 223` would panic
/// under overflow checks (and silently wrap in release), aborting on a bad file.
fn render_quality(quality: &sam::alignment::record_buf::QualityScores) -> Option<String> {
    let scores = quality.as_ref();
    if scores.is_empty() {
        None
    } else {
        Some(scores.iter().map(|&s| (s.min(93) + 33) as char).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A malformed BAM can carry a raw quality byte outside SAM's `0..=93` range
    /// (noodles does not range-check them). `render_quality` must clamp rather than
    /// overflow `u8` on `s + 33` — a byte `>= 223` used to panic under overflow
    /// checks (and wrap to a wrong char in release), aborting `read_bam` on a bad file.
    #[test]
    fn render_quality_clamps_out_of_range_bam_bytes() {
        use sam::alignment::record_buf::QualityScores;
        let q = QualityScores::from(vec![0u8, 30, 93, 200, 250, 255]);
        let rendered = render_quality(&q).expect("non-empty quality string");
        // Every rendered byte is printable ASCII `!`..`~`; the out-of-range bytes
        // (200/250/255) clamp to the max score 93 → `~`.
        assert!(
            rendered.chars().all(|c| ('!'..='~').contains(&c)),
            "non-printable char in {rendered:?}"
        );
        assert!(rendered.ends_with("~~~"), "200/250/255 must clamp to '~': {rendered:?}");
    }

    /// Regenerate `examples/data/alignments.bam` (and its `.bai` index) from the text
    /// `alignments.sam` using noodles' BAM writer and the `bam::fs::index` helper (no
    /// external `samtools`). Ignored by default; run on demand with
    /// `cargo test --bin helix generate_bam_fixture -- --ignored` whenever the SAM
    /// fixture changes. The `.bai` is what `read_bam(path, region)` seeks with; it
    /// requires the BAM be coordinate-sorted (mapped reads by position, unmapped last).
    #[test]
    #[ignore]
    fn generate_bam_fixture() {
        use sam::alignment::io::Write as _;

        // Write the BAM in a scope so the writer flushes and the file closes before
        // `bam::fs::index` reopens it to build the index.
        {
            let inner = crate::bioio::open_maybe_gzip("examples/data/alignments.sam").unwrap();
            let mut reader = sam::io::Reader::new(inner);
            let header = reader.read_header().unwrap();

            let out = std::fs::File::create("examples/data/alignments.bam").unwrap();
            let mut writer = bam::io::Writer::new(out);
            writer.write_header(&header).unwrap();
            for result in reader.record_bufs(&header) {
                let record = result.unwrap();
                writer.write_alignment_record(&header, &record).unwrap();
            }
            writer.try_finish().unwrap();
        }

        let index = bam::fs::index("examples/data/alignments.bam").unwrap();
        let bai_out = std::fs::File::create("examples/data/alignments.bam.bai").unwrap();
        let mut bai_writer = bam::bai::io::Writer::new(bai_out);
        bai_writer.write_index(&index).unwrap();
    }
}
