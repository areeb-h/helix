//! The reader: classify every leaf column once, then one typed pass per row
//! group. Definition levels reconstruct nulls; dictionary encoding is already
//! resolved below this API. Nested schemas are refused at the ROOT (optional
//! groups raise def levels without repetition, so checking rep levels alone
//! would miss them).

use std::rc::Rc;

use parquet::basic::{ConvertedType, LogicalType, Repetition, TimeUnit, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::schema::types::ColumnDescriptor;

use crate::backend::Df;
use crate::error::HelixError;
use crate::value::Value;

use super::super::columns::Col;
use super::super::NativeFrame;
use super::foreign;
use super::pq_err;

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

/// Read `count` typed values + def levels from one column reader, appending
/// cells (missing where def == 0). One closure per physical type keeps the
/// def-level walk in exactly one place.
fn drain<T: parquet::data_type::DataType>(
    r: &mut parquet::column::reader::ColumnReaderImpl<T>,
    optional: bool,
    rows: usize,
    mut cell: impl FnMut(&T::T) -> Result<Value, HelixError>,
    out: &mut Vec<Value>,
) -> Result<(), HelixError> {
    let mut values: Vec<T::T> = Vec::with_capacity(rows);
    let mut defs: Vec<i16> = Vec::with_capacity(if optional { rows } else { 0 });
    loop {
        let (records, _, _) = r
            .read_records(8192, optional.then_some(&mut defs), None, &mut values)
            .map_err(|e| HelixError::new(format!("parquet read failed: {e}"), 0, 0))?;
        if records == 0 {
            break;
        }
    }
    if optional {
        let mut vi = values.iter();
        for &d in &defs {
            if d > 0 {
                let v = vi.next().ok_or_else(|| {
                    HelixError::new("parquet definition levels disagree with values", 0, 0)
                })?;
                out.push(cell(v)?);
            } else {
                out.push(Value::Missing);
            }
        }
    } else {
        for v in &values {
            out.push(cell(v)?);
        }
    }
    Ok(())
}

pub fn read_parquet(path: &str, line: usize, col: usize) -> Result<Df, HelixError> {
    let file = std::fs::File::open(path)
        .map_err(|e| pq_err("open", path, e, line, col))?;
    let reader =
        SerializedFileReader::new(file).map_err(|e| pq_err("read", path, e, line, col))?;
    let meta = reader.metadata();
    let schema = meta.file_metadata().schema_descr();

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

    let mut cells: Vec<Vec<Value>> = vec![Vec::new(); schema.num_columns()];
    for rg_idx in 0..reader.num_row_groups() {
        let rg = reader.get_row_group(rg_idx).map_err(|e| pq_err("read", path, e, line, col))?;
        let rows = rg.metadata().num_rows() as usize;
        for ci in 0..schema.num_columns() {
            let desc = schema.column(ci);
            let optional = desc.max_def_level() > 0;
            let cr = rg.get_column_reader(ci).map_err(|e| pq_err("read", path, e, line, col))?;
            let out = &mut cells[ci];
            let kind = &kinds[ci];
            let res = match cr {
                ColumnReader::BoolColumnReader(mut r) => {
                    drain(&mut r, optional, rows, |v| Ok(Value::Bool(*v)), out)
                }
                ColumnReader::Int64ColumnReader(mut r) => match kind {
                    Kind::Timestamp { per_sec, width, utc } => {
                        let (p, w, u) = (*per_sec, *width, *utc);
                        drain(&mut r, optional, rows, |v| {
                            let mut s = foreign::timestamp_str(*v, p, w);
                            if u {
                                s.push_str(" UTC");
                            }
                            Ok(Value::Str(Rc::new(s)))
                        }, out)
                    }
                    Kind::Time { per_sec, width } => {
                        let (p, w) = (*per_sec, *width);
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::time_str(*v, p, w))))
                        }, out)
                    }
                    Kind::Decimal { scale } => {
                        let s = *scale;
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::decimal_str(*v as i128, s))))
                        }, out)
                    }
                    // Signed and unsigned both pass through the i64 payload —
                    // the bridge wraps u64 > i64::MAX the same way.
                    _ => drain(&mut r, optional, rows, |v| Ok(Value::Int(*v)), out),
                },
                ColumnReader::Int32ColumnReader(mut r) => match kind {
                    Kind::Date => drain(&mut r, optional, rows, |v| {
                        Ok(Value::Str(Rc::new(foreign::date_str(*v))))
                    }, out),
                    Kind::Time { per_sec, width } => {
                        let (p, w) = (*per_sec, *width);
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::time_str(*v as i64, p, w))))
                        }, out)
                    }
                    Kind::Decimal { scale } => {
                        let s = *scale;
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::decimal_str(*v as i128, s))))
                        }, out)
                    }
                    Kind::UintWiden => drain(&mut r, optional, rows, |v| {
                        Ok(Value::Int((*v as u32) as i64))
                    }, out),
                    _ => drain(&mut r, optional, rows, |v| Ok(Value::Int(*v as i64)), out),
                },
                ColumnReader::Int96ColumnReader(mut r) => drain(&mut r, optional, rows, |v| {
                    Ok(Value::Str(Rc::new(foreign::timestamp_str(
                        v.to_nanos(),
                        1_000_000_000,
                        9,
                    ))))
                }, out),
                ColumnReader::FloatColumnReader(mut r) => {
                    drain(&mut r, optional, rows, |v| Ok(Value::Float(*v as f64)), out)
                }
                ColumnReader::DoubleColumnReader(mut r) => {
                    drain(&mut r, optional, rows, |v| Ok(Value::Float(*v)), out)
                }
                ColumnReader::ByteArrayColumnReader(mut r) => match kind {
                    Kind::Decimal { scale } => {
                        let s = *scale;
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::decimal_str(
                                foreign::be_bytes_to_i128(v.data()),
                                s,
                            ))))
                        }, out)
                    }
                    _ => {
                        let name = desc.name().to_string();
                        drain(&mut r, optional, rows, move |v| {
                            let s = v.as_utf8().map_err(|_| {
                                HelixError::new(
                                    format!("parquet column `{name}` holds non-UTF-8 bytes"),
                                    0,
                                    0,
                                )
                            })?;
                            Ok(Value::Str(Rc::new(s.to_string())))
                        }, out)
                    }
                },
                ColumnReader::FixedLenByteArrayColumnReader(mut r) => match kind {
                    Kind::Decimal { scale } => {
                        let s = *scale;
                        drain(&mut r, optional, rows, |v| {
                            Ok(Value::Str(Rc::new(foreign::decimal_str(
                                foreign::be_bytes_to_i128(v.data()),
                                s,
                            ))))
                        }, out)
                    }
                    // classify() refused every other FLBA shape already.
                    _ => Err(HelixError::new("unreachable FLBA kind", line, col)),
                },
            };
            res.map_err(|e| {
                // The drain helpers have no path context — restore it here.
                if e.message.starts_with("parquet") || e.message.starts_with("could not") {
                    e
                } else {
                    pq_err("read", path, e.message.clone(), line, col)
                }
            })?;
        }
    }

    let cols: Vec<(String, Col)> = (0..schema.num_columns())
        .map(|i| {
            let name = schema.column(i).name().to_string();
            Col::from_values(&name, &cells[i], line, col).map(|c| (name, c))
        })
        .collect::<Result<_, _>>()?;
    NativeFrame::new(cols, line, col).map(|f| Rc::new(f) as Df)
}
