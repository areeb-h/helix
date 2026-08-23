//! The writer — columns encode and compress IN PARALLEL, each into its own
//! buffer via the crate's documented splice pattern (`get_column_writer` over a
//! `SerializedPageWriter`, stitched with `append_column`), and string columns
//! bypass the value-at-a-time API entirely: the engine's columns are already
//! dictionary-encoded, so their pages are hand-built — the dictionary page is
//! the dict's text, the data page is our u32 codes through the spec's RLE
//! hybrid (`rle.rs`) — no 5M-value loop anywhere. zstd level 3, the sibling
//! engine's own default, so either engine opens the other's files (proven by
//! the cross-engine tests both ways).

use std::sync::Arc;

use parquet::basic::{Compression, Encoding, LogicalType, Repetition, Type as PhysicalType, ZstdLevel};
use parquet::column::page::{CompressedPage, Page, PageWriter};
use parquet::column::writer::{get_column_writer, ColumnCloseResult, ColumnWriter};
use parquet::file::metadata::ColumnChunkMetaData;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterPropertiesPtr};
use parquet::file::writer::{SerializedFileWriter, SerializedPageWriter, TrackedWrite};
use parquet::schema::types::{ColumnDescPtr, SchemaDescriptor, Type, TypePtr};
use rayon::prelude::*;

use crate::error::HelixError;

use super::super::columns::Col;
use super::super::NativeFrame;
use super::pq_err;
use super::rle;

/// A Send view of one column — workers never see an `Rc`.
enum WView<'a> {
    I(&'a [i64], &'a [bool]),
    F(&'a [f64], &'a [bool]),
    B(&'a [bool], &'a [bool]),
    S { dict: Vec<&'a str>, codes: &'a [u32], valid: &'a [bool] },
    N(usize),
}

pub fn write_parquet(
    frame: &NativeFrame,
    path: &str,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let werr = |e: parquet::errors::ParquetError| pq_err("write", path, e, line, col);

    let fields: Vec<TypePtr> = frame
        .columns()
        .iter()
        .map(|(name, c)| {
            let builder = match c {
                Col::I64 { .. } | Col::Null { .. } => {
                    // An all-missing column still needs a dtype on disk; Int64 is
                    // the least surprising carrier for pure nulls.
                    Type::primitive_type_builder(name, PhysicalType::INT64)
                }
                Col::F64 { .. } => Type::primitive_type_builder(name, PhysicalType::DOUBLE),
                Col::Bool { .. } => Type::primitive_type_builder(name, PhysicalType::BOOLEAN),
                Col::Str { .. } => Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                    .with_logical_type(Some(LogicalType::String)),
            };
            builder
                .with_repetition(Repetition::OPTIONAL)
                .build()
                .map(Arc::new)
                .map_err(werr)
        })
        .collect::<Result<_, _>>()?;
    let schema = Type::group_type_builder("schema")
        .with_fields(fields)
        .build()
        .map(Arc::new)
        .map_err(werr)?;
    let descrs = SchemaDescriptor::new(schema.clone());

    // Zstd level 3 = what polars' default writes; try_new(3) cannot fail for a
    // constant inside 1..=22, and the fallback keeps this panic-free anyway.
    let level = ZstdLevel::try_new(3).unwrap_or_default();
    let props: WriterPropertiesPtr = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::ZSTD(level))
            // 1024-value write batches meant ~5000 per-batch passes per 5M-row
            // column; one big batch amortizes the machinery.
            .set_write_batch_size(1 << 20)
            // Chunk-level statistics: per-PAGE min/max of string pages bloated
            // the metadata 3.4x and cost encode time.
            .set_statistics_enabled(EnabledStatistics::Chunk)
            // Uniformly none: the hand-built string chunk carries no offset
            // index, and the crate's footer writer panics on a Some/None mix
            // (its own FIXME). The index is an optional page-pruning aid.
            .set_offset_index_disabled(true)
            .build(),
    );

    let views: Vec<WView> = frame
        .columns()
        .iter()
        .map(|(_, c)| match c {
            Col::I64 { vals, valid } => WView::I(vals, valid),
            Col::F64 { vals, valid } => WView::F(vals, valid),
            Col::Bool { vals, valid } => WView::B(vals, valid),
            Col::Str { dict, codes, valid } => WView::S {
                dict: dict.iter().map(|s| s.as_str()).collect(),
                codes,
                valid,
            },
            Col::Null { len } => WView::N(*len),
        })
        .collect();

    // Every column encodes and compresses on its own worker.
    let encoded: Vec<Result<(Vec<u8>, ColumnCloseResult), String>> = views
        .par_iter()
        .enumerate()
        .map(|(i, view)| match view {
            WView::S { dict, codes, valid } => {
                encode_str_chunk(descrs.column(i), dict, codes, valid, level)
            }
            _ => encode_standard(descrs.column(i), props.clone(), view),
        })
        .collect();

    let file = std::fs::File::create(path).map_err(|e| pq_err("create", path, e, line, col))?;
    let mut writer = SerializedFileWriter::new(file, schema, props).map_err(werr)?;
    if frame.len() > 0 {
        let mut rg = writer.next_row_group().map_err(werr)?;
        for chunk in encoded {
            let (buf, close) = chunk.map_err(|m| pq_err("write", path, m, line, col))?;
            rg.append_column(&bytes::Bytes::from(buf), close).map_err(werr)?;
        }
        rg.close().map_err(werr)?;
    }
    writer.close().map_err(werr)?;
    Ok(())
}

/// The crate's own encoder, pointed at a private buffer (numeric/bool/null
/// columns — their value loop is already fast).
fn encode_standard(
    descr: ColumnDescPtr,
    props: WriterPropertiesPtr,
    view: &WView,
) -> Result<(Vec<u8>, ColumnCloseResult), String> {
    let e = |e: parquet::errors::ParquetError| format!("column encode failed: {e}");
    let mut track = TrackedWrite::new(Vec::new());
    let close = {
        let pw = SerializedPageWriter::new(&mut track);
        let cw = get_column_writer(descr, props, Box::new(pw));
        match (cw, view) {
            (ColumnWriter::Int64ColumnWriter(mut t), WView::I(vals, valid)) => {
                let (present, defs) = split(vals.iter().copied(), valid);
                t.write_batch(&present, Some(&defs), None).map_err(e)?;
                t.close().map_err(e)?
            }
            (ColumnWriter::Int64ColumnWriter(mut t), WView::N(len)) => {
                let defs = vec![0i16; *len];
                t.write_batch(&[], Some(&defs), None).map_err(e)?;
                t.close().map_err(e)?
            }
            (ColumnWriter::DoubleColumnWriter(mut t), WView::F(vals, valid)) => {
                let (present, defs) = split(vals.iter().copied(), valid);
                t.write_batch(&present, Some(&defs), None).map_err(e)?;
                t.close().map_err(e)?
            }
            (ColumnWriter::BoolColumnWriter(mut t), WView::B(vals, valid)) => {
                let (present, defs) = split(vals.iter().copied(), valid);
                t.write_batch(&present, Some(&defs), None).map_err(e)?;
                t.close().map_err(e)?
            }
            _ => return Err("column writer/view mismatch".to_string()),
        }
    };
    let buf = track.into_inner().map_err(e)?;
    Ok((buf, close))
}

/// The dictionary column bypass: hand-built pages straight from the engine's
/// own representation. The dictionary page is the dict's text; the single data
/// page is RLE def levels + RLE dictionary codes — bytes proportional to the
/// DISTINCT values plus a few bits per row, never a per-value loop.
fn encode_str_chunk(
    descr: ColumnDescPtr,
    dict: &[&str],
    codes: &[u32],
    valid: &[bool],
    level: ZstdLevel,
) -> Result<(Vec<u8>, ColumnCloseResult), String> {
    let e = |e: parquet::errors::ParquetError| format!("column encode failed: {e}");
    let z = |m: std::io::Error| format!("zstd failed: {m}");
    let rows = valid.len();

    let mut track = TrackedWrite::new(Vec::new());
    let (spec_dict, spec_data) = {
        let mut pw = SerializedPageWriter::new(&mut track);

        // Dictionary page: PLAIN byte arrays — [u32 LE length][bytes] each.
        let mut draw =
            Vec::with_capacity(dict.iter().map(|s| s.len() + 4).sum::<usize>());
        for s in dict {
            draw.extend_from_slice(&(s.len() as u32).to_le_bytes());
            draw.extend_from_slice(s.as_bytes());
        }
        let dcomp = zstd::bulk::compress(&draw, level.compression_level()).map_err(z)?;
        let dict_page = Page::DictionaryPage {
            buf: bytes::Bytes::from(dcomp),
            num_values: dict.len() as u32,
            encoding: Encoding::PLAIN,
            is_sorted: false,
        };
        let spec_dict =
            pw.write_page(CompressedPage::new(dict_page, draw.len())).map_err(e)?;

        // Data page (V1): [u32 LE len][RLE def levels] then [u8 width][RLE codes].
        let mut praw = Vec::new();
        let levels: Vec<u32> = valid.iter().map(|b| u32::from(*b)).collect();
        let mut lvl = Vec::new();
        rle::encode(&levels, 1, &mut lvl);
        praw.extend_from_slice(&(lvl.len() as u32).to_le_bytes());
        praw.extend_from_slice(&lvl);
        let width = rle::width_for(dict.len().saturating_sub(1) as u32);
        praw.push(width);
        let present: Vec<u32> =
            codes.iter().zip(valid).filter(|(_, ok)| **ok).map(|(c, _)| *c).collect();
        rle::encode(&present, width, &mut praw);
        let pcomp = zstd::bulk::compress(&praw, level.compression_level()).map_err(z)?;
        let data_page = Page::DataPage {
            buf: bytes::Bytes::from(pcomp),
            num_values: rows as u32,
            encoding: Encoding::RLE_DICTIONARY,
            def_level_encoding: Encoding::RLE,
            rep_level_encoding: Encoding::RLE,
            statistics: None,
        };
        let spec_data =
            pw.write_page(CompressedPage::new(data_page, praw.len())).map_err(e)?;
        (spec_dict, spec_data)
    };

    let total = track.bytes_written();
    let buf = track.into_inner().map_err(e)?;
    let metadata = ColumnChunkMetaData::builder(descr)
        .set_compression(Compression::ZSTD(level))
        .set_encodings(vec![Encoding::PLAIN, Encoding::RLE, Encoding::RLE_DICTIONARY])
        .set_num_values(rows as i64)
        .set_total_compressed_size(total as i64)
        .set_total_uncompressed_size(
            (spec_dict.uncompressed_size + spec_data.uncompressed_size) as i64,
        )
        .set_dictionary_page_offset(Some(spec_dict.offset as i64))
        .set_data_page_offset(spec_data.offset as i64)
        .build()
        .map_err(e)?;
    Ok((
        buf,
        ColumnCloseResult {
            bytes_written: total as u64,
            rows_written: rows as u64,
            metadata,
            bloom_filter: None,
            column_index: None,
            offset_index: None,
        },
    ))
}

/// The mask split every dtype shares: present values in order + 1/0 def levels.
fn split<T>(vals: impl Iterator<Item = T>, valid: &[bool]) -> (Vec<T>, Vec<i16>) {
    let mut present = Vec::with_capacity(valid.len());
    for (v, ok) in vals.zip(valid) {
        if *ok {
            present.push(v);
        }
    }
    let defs: Vec<i16> = valid.iter().map(|ok| i16::from(*ok)).collect();
    (present, defs)
}
