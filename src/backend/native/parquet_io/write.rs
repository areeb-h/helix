//! The writer: one row group, every column OPTIONAL, zstd level 3 — the same
//! codec and level the polars engine writes, so either engine opens the other's
//! files. Definition levels carry the validity mask (1 = present, 0 = missing);
//! the values buffer holds only the present values, in order.

use std::sync::Arc;

use parquet::basic::{Compression, LogicalType, Repetition, Type as PhysicalType, ZstdLevel};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::{Type, TypePtr};

use crate::error::HelixError;

use super::super::columns::Col;
use super::super::NativeFrame;
use super::pq_err;

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

    // Zstd level 3 = what polars' default writes; try_new(3) cannot fail for a
    // constant inside 1..=22, and the fallback keeps this panic-free anyway.
    let level = ZstdLevel::try_new(3).unwrap_or_default();
    let props = Arc::new(
        WriterProperties::builder().set_compression(Compression::ZSTD(level)).build(),
    );
    let file = std::fs::File::create(path).map_err(|e| pq_err("create", path, e, line, col))?;
    let mut writer = SerializedFileWriter::new(file, schema, props).map_err(werr)?;

    // An empty frame closes without a row group — a valid file carrying only
    // the schema, which the reader turns back into 0-row columns.
    if frame.len() > 0 {
        let mut rg = writer.next_row_group().map_err(werr)?;
        for (_, c) in frame.columns() {
            let mut cw = rg
                .next_column()
                .map_err(werr)?
                .ok_or_else(|| pq_err("write", path, "column writer exhausted", line, col))?;
            match c {
                Col::I64 { vals, valid } => {
                    let (present, defs) = split(vals.iter().copied(), valid);
                    cw.typed::<Int64Type>()
                        .write_batch(&present, Some(&defs), None)
                        .map_err(werr)?;
                }
                Col::Null { len } => {
                    let defs = vec![0i16; *len];
                    cw.typed::<Int64Type>()
                        .write_batch(&[], Some(&defs), None)
                        .map_err(werr)?;
                }
                Col::F64 { vals, valid } => {
                    let (present, defs) = split(vals.iter().copied(), valid);
                    cw.typed::<DoubleType>()
                        .write_batch(&present, Some(&defs), None)
                        .map_err(werr)?;
                }
                Col::Bool { vals, valid } => {
                    let (present, defs) = split(vals.iter().copied(), valid);
                    cw.typed::<BoolType>()
                        .write_batch(&present, Some(&defs), None)
                        .map_err(werr)?;
                }
                Col::Str { vals, valid } => {
                    let present: Vec<ByteArray> = vals
                        .iter()
                        .zip(valid)
                        .filter(|(_, ok)| **ok)
                        .map(|(s, _)| ByteArray::from(s.as_str().as_bytes().to_vec()))
                        .collect();
                    let defs: Vec<i16> = valid.iter().map(|ok| i16::from(*ok)).collect();
                    cw.typed::<ByteArrayType>()
                        .write_batch(&present, Some(&defs), None)
                        .map_err(werr)?;
                }
            }
            cw.close().map_err(werr)?;
        }
        rg.close().map_err(werr)?;
    }
    writer.close().map_err(werr)?;
    Ok(())
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
