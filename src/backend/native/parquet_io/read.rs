//! The reader: classify every leaf column once, then decode COLUMNS IN
//! PARALLEL — the file loads into `Bytes` once and every column task opens its
//! own footer-cheap reader over a zero-copy clone. Each task produces a typed,
//! Send segment (strings hash-cons into a per-column dictionary of plain
//! `String`s); the main thread wraps dictionaries into the engine's `Rc` form.
//! Definition levels reconstruct nulls; dictionary encoding resolves below this
//! API. Nested schemas are refused at the ROOT (optional groups raise def
//! levels without repetition, so checking rep levels alone would miss them).

use std::collections::HashMap;
use std::rc::Rc;

use parquet::basic::{ConvertedType, LogicalType, Repetition, TimeUnit, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::schema::types::ColumnDescriptor;
use rayon::prelude::*;

use crate::backend::Df;
use crate::error::HelixError;

use super::super::columns::{Col, StrBuilder};
use super::super::NativeFrame;
use super::foreign;
use super::pq_err;
use super::rle;

/// How one leaf column's physical values become cells.
enum Kind {
    Int,
    /// INT32/INT64 annotated unsigned — reinterpret before widening.
    UintWiden,
    Float,
    F32Widen,
    Bool,
    Str,
    Date,
    Timestamp { per_sec: i64, width: usize, utc: bool },
    Time { per_sec: i64, width: usize },
    Decimal { scale: i32 },
    Int96,
}

fn classify(desc: &ColumnDescriptor, path: &str, line: usize, col: usize) -> Result<Kind, HelixError> {
    let unsupported = |what: &str| {
        Err(HelixError::new(
            format!("parquet `{path}` column `{}` has unsupported type {what}", desc.name()),
            line,
            col,
        )
        .hint("the native engine reads int/float/bool/string columns natively and \
               date/timestamp/time/decimal as text."))
    };
    let ts = |unit: &TimeUnit, utc: bool| match unit {
        TimeUnit::MILLIS => Kind::Timestamp { per_sec: 1_000, width: 3, utc },
        TimeUnit::MICROS => Kind::Timestamp { per_sec: 1_000_000, width: 6, utc },
        TimeUnit::NANOS => Kind::Timestamp { per_sec: 1_000_000_000, width: 9, utc },
    };
    Ok(match desc.physical_type() {
        PhysicalType::BOOLEAN => Kind::Bool,
        PhysicalType::DOUBLE => Kind::Float,
        PhysicalType::FLOAT => Kind::F32Widen,
        PhysicalType::INT64 => match desc.logical_type_ref() {
            Some(LogicalType::Integer(t)) if !t.is_signed => Kind::UintWiden,
            Some(LogicalType::Timestamp(t)) => ts(&t.unit, t.is_adjusted_to_u_t_c),
            Some(LogicalType::Time(t)) => match t.unit {
                TimeUnit::MICROS => Kind::Time { per_sec: 1_000_000, width: 6 },
                _ => Kind::Time { per_sec: 1_000_000_000, width: 9 },
            },
            Some(LogicalType::Decimal(d)) => Kind::Decimal { scale: d.scale },
            _ => match desc.converted_type() {
                ConvertedType::TIMESTAMP_MILLIS => ts(&TimeUnit::MILLIS, false),
                ConvertedType::TIMESTAMP_MICROS => ts(&TimeUnit::MICROS, false),
                ConvertedType::DECIMAL => Kind::Decimal { scale: desc.type_scale() },
                _ => Kind::Int,
            },
        },
        PhysicalType::INT32 => match desc.logical_type_ref() {
            Some(LogicalType::Date) => Kind::Date,
            Some(LogicalType::Integer(t)) if !t.is_signed => Kind::UintWiden,
            Some(LogicalType::Time(_)) => Kind::Time { per_sec: 1_000, width: 3 },
            Some(LogicalType::Decimal(d)) => Kind::Decimal { scale: d.scale },
            _ => match desc.converted_type() {
                ConvertedType::DATE => Kind::Date,
                ConvertedType::TIME_MILLIS => Kind::Time { per_sec: 1_000, width: 3 },
                ConvertedType::DECIMAL => Kind::Decimal { scale: desc.type_scale() },
                _ => Kind::Int,
            },
        },
        PhysicalType::INT96 => Kind::Int96,
        PhysicalType::BYTE_ARRAY => match desc.logical_type_ref() {
            Some(LogicalType::Decimal(d)) => Kind::Decimal { scale: d.scale },
            _ if desc.converted_type() == ConvertedType::DECIMAL => {
                Kind::Decimal { scale: desc.type_scale() }
            }
            _ => Kind::Str,
        },
        PhysicalType::FIXED_LEN_BYTE_ARRAY => match desc.logical_type_ref() {
            Some(LogicalType::Decimal(d)) => Kind::Decimal { scale: d.scale },
            _ if desc.converted_type() == ConvertedType::DECIMAL => {
                Kind::Decimal { scale: desc.type_scale() }
            }
            _ => return unsupported("fixed-length binary"),
        },
    })
}

/// A column decoded on a worker thread — everything here is Send; the engine's
/// `Rc` wrapping happens on the main thread.
enum SendCol {
    I64(Vec<i64>, Vec<bool>),
    F64(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Str { dict: Vec<String>, codes: Vec<u32>, valid: Vec<bool> },
}

/// Read one column's typed values + def levels across all row groups,
/// reconstructing the validity mask. `push` sees each PRESENT value in row
/// order; `gap` records a missing row.
fn drain_typed<T: parquet::data_type::DataType>(
    r: &mut parquet::column::reader::ColumnReaderImpl<T>,
    optional: bool,
    rows: usize,
    mut cell: impl FnMut(Option<&T::T>) -> Result<(), String>,
) -> Result<(), String> {
    let mut values: Vec<T::T> = Vec::with_capacity(rows);
    let mut defs: Vec<i16> = Vec::with_capacity(if optional { rows } else { 0 });
    loop {
        let (records, _, _) = r
            .read_records(16 * 1024, optional.then_some(&mut defs), None, &mut values)
            .map_err(|e| format!("parquet read failed: {e}"))?;
        if records == 0 {
            break;
        }
        if optional {
            let mut vi = values.iter();
            for &d in &defs {
                if d > 0 {
                    let v = vi
                        .next()
                        .ok_or("parquet definition levels disagree with values")?;
                    cell(Some(v))?;
                } else {
                    cell(None)?;
                }
            }
            defs.clear();
        } else {
            for v in &values {
                cell(Some(v))?;
            }
        }
        values.clear();
    }
    Ok(())
}

/// Try the page-level fast path for a plain string column: stream each row
/// group's DICTIONARY page straight into a dict + RLE-decode the data pages'
/// codes and definition levels. Answers `None` (fall back to the value-level
/// reader) for anything but the standard shape: a dict page followed by V1
/// data pages in RLE_DICTIONARY/PLAIN_DICTIONARY encoding with RLE def levels.
fn read_str_pages(
    reader: &SerializedFileReader<bytes::Bytes>,
    ci: usize,
    optional: bool,
) -> Result<Option<SendCol>, String> {
    use parquet::basic::Encoding;
    use parquet::column::page::Page;

    let mut dict: Vec<String> = Vec::new();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut codes: Vec<u32> = Vec::new();
    let mut valid: Vec<bool> = Vec::new();

    for rg_idx in 0..reader.num_row_groups() {
        let rg = reader.get_row_group(rg_idx).map_err(|e| format!("row group: {e}"))?;
        let mut pages =
            rg.get_column_page_reader(ci).map_err(|e| format!("page reader: {e}"))?;
        // Per-row-group dictionary, remapped to the global one per DISTINCT.
        let mut local: Vec<u32> = Vec::new();
        let mut saw_dict = false;
        while let Some(page) = pages.get_next_page().map_err(|e| format!("page: {e}"))? {
            match page {
                Page::DictionaryPage { buf, num_values, encoding, .. } => {
                    if !matches!(encoding, Encoding::PLAIN | Encoding::PLAIN_DICTIONARY) {
                        return Ok(None);
                    }
                    saw_dict = true;
                    local.clear();
                    local.reserve(num_values as usize);
                    let mut pos = 0usize;
                    for _ in 0..num_values {
                        let lenb = buf
                            .get(pos..pos + 4)
                            .ok_or("dictionary page ends inside a length")?;
                        let len =
                            u32::from_le_bytes([lenb[0], lenb[1], lenb[2], lenb[3]]) as usize;
                        pos += 4;
                        let s = buf
                            .get(pos..pos + len)
                            .ok_or("dictionary page ends inside a value")?;
                        pos += len;
                        let s = std::str::from_utf8(s)
                            .map_err(|_| "column holds non-UTF-8 bytes".to_string())?;
                        let g = match index.get(s) {
                            Some(&g) => g,
                            None => {
                                let g = dict.len() as u32;
                                index.insert(s.to_string(), g);
                                dict.push(s.to_string());
                                g
                            }
                        };
                        local.push(g);
                    }
                }
                Page::DataPage {
                    buf,
                    num_values,
                    encoding,
                    def_level_encoding,
                    ..
                } => {
                    if !saw_dict
                        || !matches!(
                            encoding,
                            Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY
                        )
                    {
                        return Ok(None);
                    }
                    let n = num_values as usize;
                    let mut pos = 0usize;
                    let mut present = n;
                    let defs_start = valid.len();
                    if optional {
                        if def_level_encoding != Encoding::RLE {
                            return Ok(None);
                        }
                        let lenb =
                            buf.get(0..4).ok_or("data page ends inside the level length")?;
                        let dlen =
                            u32::from_le_bytes([lenb[0], lenb[1], lenb[2], lenb[3]]) as usize;
                        pos = 4 + dlen;
                        let mut levels: Vec<u32> = Vec::with_capacity(n);
                        rle::decode(
                            buf.get(4..4 + dlen).ok_or("data page ends inside levels")?,
                            1,
                            n,
                            &mut levels,
                        )?;
                        present = 0;
                        for l in levels {
                            valid.push(l == 1);
                            if l == 1 {
                                present += 1;
                            }
                        }
                    } else {
                        valid.extend(std::iter::repeat_n(true, n));
                    }
                    let width = *buf.get(pos).ok_or("data page ends before the bit width")?;
                    if width > 32 {
                        return Err("dictionary index width exceeds 32 bits".to_string());
                    }
                    let mut page_codes: Vec<u32> = Vec::with_capacity(present);
                    rle::decode(
                        buf.get(pos + 1..).ok_or("data page ends inside the codes")?,
                        width,
                        present,
                        &mut page_codes,
                    )?;
                    // Fill codes in row order, remapped through the local dict.
                    let mut pi = 0usize;
                    for &ok in &valid[defs_start..] {
                        if ok {
                            let lc = *page_codes
                                .get(pi)
                                .ok_or("fewer dictionary codes than present values")?;
                            pi += 1;
                            let g = *local
                                .get(lc as usize)
                                .ok_or("dictionary code out of range")?;
                            codes.push(g);
                        } else {
                            codes.push(0);
                        }
                    }
                }
                Page::DataPageV2 { .. } => return Ok(None),
            }
        }
    }
    Ok(Some(SendCol::Str { dict, codes, valid }))
}

/// Decode column `ci` of every row group into a Send segment. Runs on a worker.
fn read_column(
    bytes: &bytes::Bytes,
    ci: usize,
    kind: &Kind,
    total_rows: usize,
) -> Result<SendCol, String> {
    let reader =
        SerializedFileReader::new(bytes.clone()).map_err(|e| format!("re-open failed: {e}"))?;
    let schema = reader.metadata().file_metadata().schema_descr();
    let desc = schema.column(ci);
    let optional = desc.max_def_level() > 0;
    let name = desc.name().to_string();

    // Plain string columns: the page-level fast path streams dictionary pages
    // straight into the engine's own dict representation. Anything nonstandard
    // falls through to the value-level reader below.
    if matches!(kind, Kind::Str)
        && let Some(seg) = read_str_pages(&reader, ci, optional)?
    {
        return Ok(seg);
    }

    // Accumulators — exactly one becomes the result, per the classified kind.
    let mut ivals = Vec::new();
    let mut fvals = Vec::new();
    let mut bvals = Vec::new();
    let mut dict: Vec<String> = Vec::new();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut codes: Vec<u32> = Vec::new();
    let mut valid = Vec::with_capacity(total_rows);

    macro_rules! cons {
        ($s:expr, $valid:expr, $codes:expr) => {{
            let s: String = $s;
            let code = match index.get(s.as_str()) {
                Some(&c) => c,
                None => {
                    let c = dict.len() as u32;
                    index.insert(s.clone(), c);
                    dict.push(s);
                    c
                }
            };
            $codes.push(code);
            $valid.push(true);
        }};
    }

    for rg_idx in 0..reader.num_row_groups() {
        let rg = reader.get_row_group(rg_idx).map_err(|e| format!("row group: {e}"))?;
        let rows = rg.metadata().num_rows() as usize;
        let cr = rg.get_column_reader(ci).map_err(|e| format!("column reader: {e}"))?;
        match cr {
            ColumnReader::BoolColumnReader(mut r) => drain_typed(
                &mut r,
                optional,
                rows,
                |o| match o {
                    Some(v) => {
                    bvals.push(*v);
                    valid.push(true);
                    Ok(())
                    }
                    None => {
                    bvals.push(false);
                    valid.push(false);
                        Ok(())
                    }
                },
            )?,
            ColumnReader::Int64ColumnReader(mut r) => match kind {
                Kind::Timestamp { per_sec, width, utc } => {
                    let (p, w, u) = (*per_sec, *width, *utc);
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            let mut s = foreign::timestamp_str(*v, p, w);
                            if u {
                                s.push_str(" UTC");
                            }
                            cons!(s, valid, codes);
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                Kind::Time { per_sec, width } => {
                    let (p, w) = (*per_sec, *width);
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(foreign::time_str(*v, p, w), valid, codes);
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                Kind::Decimal { scale } => {
                    let sc = *scale;
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(foreign::decimal_str(*v as i128, sc), valid, codes);
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                _ => drain_typed(
                    &mut r,
                    optional,
                    rows,
                    |o| match o {
                        Some(v) => {
                        ivals.push(*v);
                        valid.push(true);
                        Ok(())
                        }
                        None => {
                        ivals.push(0);
                        valid.push(false);
                            Ok(())
                        }
                    },
                )?,
            },
            ColumnReader::Int32ColumnReader(mut r) => match kind {
                Kind::Date => drain_typed(
                    &mut r,
                    optional,
                    rows,
                    |o| match o {
                        Some(v) => {
                        cons!(foreign::date_str(*v), valid, codes);
                        Ok(())
                        }
                        None => {
                        codes.push(0);
                        valid.push(false);
                            Ok(())
                        }
                    },
                )?,
                Kind::Time { per_sec, width } => {
                    let (p, w) = (*per_sec, *width);
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(foreign::time_str(*v as i64, p, w), valid, codes);
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                Kind::Decimal { scale } => {
                    let sc = *scale;
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(foreign::decimal_str(*v as i128, sc), valid, codes);
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                Kind::UintWiden => drain_typed(
                    &mut r,
                    optional,
                    rows,
                    |o| match o {
                        Some(v) => {
                        ivals.push((*v as u32) as i64);
                        valid.push(true);
                        Ok(())
                        }
                        None => {
                        ivals.push(0);
                        valid.push(false);
                            Ok(())
                        }
                    },
                )?,
                _ => drain_typed(
                    &mut r,
                    optional,
                    rows,
                    |o| match o {
                        Some(v) => {
                        ivals.push(*v as i64);
                        valid.push(true);
                        Ok(())
                        }
                        None => {
                        ivals.push(0);
                        valid.push(false);
                            Ok(())
                        }
                    },
                )?,
            },
            ColumnReader::Int96ColumnReader(mut r) => drain_typed(
                &mut r,
                optional,
                rows,
                |o| match o {
                    Some(v) => {
                    cons!(
                        foreign::timestamp_str(v.to_nanos(), 1_000_000_000, 9),
                        valid,
                        codes
                    );
                    Ok(())
                    }
                    None => {
                    codes.push(0);
                    valid.push(false);
                        Ok(())
                    }
                },
            )?,
            ColumnReader::FloatColumnReader(mut r) => drain_typed(
                &mut r,
                optional,
                rows,
                |o| match o {
                    Some(v) => {
                    fvals.push(*v as f64);
                    valid.push(true);
                    Ok(())
                    }
                    None => {
                    fvals.push(0.0);
                    valid.push(false);
                        Ok(())
                    }
                },
            )?,
            ColumnReader::DoubleColumnReader(mut r) => drain_typed(
                &mut r,
                optional,
                rows,
                |o| match o {
                    Some(v) => {
                    fvals.push(*v);
                    valid.push(true);
                    Ok(())
                    }
                    None => {
                    fvals.push(0.0);
                    valid.push(false);
                        Ok(())
                    }
                },
            )?,
            ColumnReader::ByteArrayColumnReader(mut r) => match kind {
                Kind::Decimal { scale } => {
                    let sc = *scale;
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(
                                foreign::decimal_str(foreign::be_bytes_to_i128(v.data()), sc),
                                valid,
                                codes
                            );
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                _ => drain_typed(
                    &mut r,
                    optional,
                    rows,
                    |o| match o {
                        Some(v) => {
                        let s = v
                            .as_utf8()
                            .map_err(|_| format!("column `{name}` holds non-UTF-8 bytes"))?;
                        // Hash-cons without allocating on dictionary hits.
                        match index.get(s) {
                            Some(&c) => {
                                codes.push(c);
                                valid.push(true);
                            }
                            None => {
                                let owned = s.to_string();
                                let c = dict.len() as u32;
                                index.insert(owned.clone(), c);
                                dict.push(owned);
                                codes.push(c);
                                valid.push(true);
                            }
                        }
                        Ok(())
                        }
                        None => {
                        codes.push(0);
                        valid.push(false);
                            Ok(())
                        }
                    },
                )?,
            },
            ColumnReader::FixedLenByteArrayColumnReader(mut r) => match kind {
                Kind::Decimal { scale } => {
                    let sc = *scale;
                    drain_typed(
                        &mut r,
                        optional,
                        rows,
                        |o| match o {
                            Some(v) => {
                            cons!(
                                foreign::decimal_str(foreign::be_bytes_to_i128(v.data()), sc),
                                valid,
                                codes
                            );
                            Ok(())
                            }
                            None => {
                            codes.push(0);
                            valid.push(false);
                                Ok(())
                            }
                        },
                    )?
                }
                _ => return Err("unreachable FLBA kind".to_string()),
            },
        }
    }

    Ok(match kind {
        Kind::Bool => SendCol::Bool(bvals, valid),
        Kind::Float | Kind::F32Widen => SendCol::F64(fvals, valid),
        Kind::Int | Kind::UintWiden => SendCol::I64(ivals, valid),
        _ => SendCol::Str { dict, codes, valid },
    })
}

pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
    let raw = std::fs::read(path).map_err(|e| pq_err("open", path, e, line, col))?;
    let bytes = bytes::Bytes::from(raw);
    let reader = SerializedFileReader::new(bytes.clone())
        .map_err(|e| pq_err("read", path, e, line, col))?;
    let meta = reader.metadata();
    let schema = meta.file_metadata().schema_descr();
    let total_rows = meta.file_metadata().num_rows().max(0) as usize;

    // Flat-schema gate, at the root.
    for field in schema.root_schema().get_fields() {
        if !field.is_primitive() || field.get_basic_info().repetition() == Repetition::REPEATED {
            return Err(HelixError::new(
                format!(
                    "parquet `{path}` column `{}` is nested (a list, map, or struct)",
                    field.name()
                ),
                line,
                col,
            )
            .hint("the native engine reads flat tables; flatten the file, or build with \
                   `--features dataframes` for the polars reader."));
        }
    }

    let kinds: Vec<Kind> = (0..schema.num_columns())
        .map(|i| classify(schema.column(i).as_ref(), path, line, col))
        .collect::<Result<_, _>>()?;
    let names: Vec<String> =
        (0..schema.num_columns()).map(|i| schema.column(i).name().to_string()).collect();

    // Decode every column in parallel; each worker re-opens the (in-memory)
    // file — a footer parse, microseconds against a column decode.
    let segs: Vec<Result<SendCol, String>> = kinds
        .par_iter()
        .enumerate()
        .map(|(ci, kind)| read_column(&bytes, ci, kind, total_rows))
        .collect();

    let mut cols: Vec<(String, Col)> = Vec::with_capacity(names.len());
    for (name, seg) in names.into_iter().zip(segs) {
        let seg = seg.map_err(|m| pq_err("read", path, m, line, col))?;
        let c = match seg {
            SendCol::I64(vals, valid) => Col::I64 { vals, valid },
            SendCol::F64(vals, valid) => Col::F64 { vals, valid },
            SendCol::Bool(vals, valid) => Col::Bool { vals, valid },
            SendCol::Str { dict, codes, valid } => {
                // The worker's dictionary wraps into the engine's Rc form —
                // per DISTINCT value, not per cell.
                let mut b = StrBuilder::with_capacity(0);
                let trans: Vec<u32> = dict.iter().map(|s| b.intern(s)).collect();
                let _ = trans; // codes are already the worker's dict order,
                               // which intern() reproduces 1:1 on a fresh builder
                b.set_codes(codes, valid);
                b.finish()
            }
        };
        cols.push((name, c));
    }
    NativeFrame::new(cols, line, col).map(|f| Rc::new(f) as Df)
}
