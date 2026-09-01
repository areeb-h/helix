//! Builtins: DataFrame constructors (columns and rows) — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.


use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_to_dataframe(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        // An ARRAY OF RECORDS is the row-oriented twin of `dataframe({cols})`
        // — the shape `db` rows and parsed JSON produce — and builds NATIVELY
        // (no Python needed). Column order is the FIRST record's field order;
        // every record must carry exactly that field set, because a frame
        // column has one length — an absent field is an error naming the row,
        // never a silent hole.
        if let Value::Array(rows) = &args[0] {
            if rows.is_empty() {
                return Err(HelixError::new(
                    "`to_dataframe` cannot infer a schema from an empty array",
                    line,
                    col,
                )
                .hint(
                        "give each column at least one value (`dataframe({a: [1]})`), or \
                         read a header-only CSV — a truly empty frame cannot state its \
                         column types yet.",
                    ));
            }
            if let Some(bad) =
                rows.iter_values().position(|r| !matches!(r, Value::Record(_)))
            {
                let it = rows.get(bad);
                return Err(HelixError::new(
                    format!(
                        "`to_dataframe` takes an array of records, but element {bad} is {}",
                        crate::value::with_article(it.type_name())
                    ),
                    line,
                    col,
                )
                .hint(
                    "rows look like `[{id: 1, v: 2.0}, {id: 2, v: 3.5}]`; \
                     column-wise data goes to `dataframe({...})`.",
                ));
            }
            let first = rows.get(0);
            let Value::Record(first_fields) = &first else { unreachable!() };
            let names: Vec<Symbol> = first_fields.iter().map(|(s, _)| *s).collect();
            let mut cols: Vec<Vec<Value>> =
                names.iter().map(|_| Vec::with_capacity(rows.len())).collect();
            for (ri, row) in rows.iter_values().enumerate() {
                let Value::Record(fields) = &row else { unreachable!() };
                if fields.len() != names.len() {
                    return Err(HelixError::new(
                        format!(
                            "`to_dataframe` row {ri} has {} fields, expected {}",
                            fields.len(),
                            names.len()
                        ),
                        line,
                        col,
                    )
                    .hint(format!(
                        "every row must carry the first row's fields: {}.",
                        names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    )));
                }
                for (ci, want) in names.iter().enumerate() {
                    match fields.iter().find(|(s, _)| s == want) {
                        Some((_, v)) => cols[ci].push(v.clone()),
                        None => {
                            return Err(HelixError::new(
                                format!(
                                    "`to_dataframe` row {ri} has no field `{}`",
                                    want.as_str()
                                ),
                                line,
                                col,
                            )
                            .hint(format!(
                                "every row must carry the first row's fields: {}.",
                                names
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )));
                        }
                    }
                }
            }
            let mut columns = Vec::with_capacity(names.len());
            for (want, vals) in names.iter().zip(cols) {
                let cname = want.as_str();
                columns.push((
                    cname.to_string(),
                    array_to_coldata(cname, &Value::array(vals), line, col)?,
                ));
            }
            return Ok(Value::dataframe(crate::backend::build_frame(columns, line, col)?));
        }
        // Bring a Python polars/pandas/pyarrow frame into Helix as a native
        // DataFrame, zero-copy via Arrow.
        crate::python::to_dataframe(args.into_iter().next().unwrap(), line, col)
    
}

#[inline]
pub(super) fn a_dataframe(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // Build a DataFrame from in-memory columns: `dataframe({age: [30, 41],
        // name: ["a", "b"]})`. Each record field is a column (its array); the
        // backend seam (`build_frame`) checks equal lengths / duplicate names.
        arity(name, &args, 1, line, col)?;
        // A DICT IS ACCEPTED AS WELL AS A RECORD, and that is not a convenience — it is the
        // only way to build a frame whose SCHEMA IS KNOWN AT RUN TIME. A record's fields are
        // syntax, so `dataframe({...})` can only ever produce columns the source text names.
        // A storage engine's chunk takes its schema from the data, so it could not build a
        // frame at all; reported from the field as a blocker, having already forced a
        // caller-supplied closure in two other places.
        //
        // COLUMN ORDER DIFFERS BETWEEN THE TWO, deliberately and visibly: a record keeps the
        // order written, a Dict is sorted by key (it is a BTreeMap), so `dataframe(dict)`
        // yields columns in sorted name order. Deterministic either way; `select` fixes an
        // order that matters. Sorting is the honest choice — a Dict has no insertion order
        // to preserve, so inventing one would be a lie about where it came from.
        let mut columns: Vec<(String, _)> = Vec::new();
        match &args[0] {
            Value::Record(fields) => {
                if fields.is_empty() {
                    return Err(HelixError::new("`dataframe` needs at least one column", line, col)
                        .hint("e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`."));
                }
                columns.reserve(fields.len());
                for (sym, val) in fields.iter() {
                    let cname = sym.as_str();
                    columns.push((cname.to_string(), array_to_coldata(cname, val, line, col)?));
                }
            }
            Value::Dict(d) => {
                let d = d.map();
                if d.is_empty() {
                    return Err(HelixError::new("`dataframe` needs at least one column", line, col)
                        .hint("an empty dict has no columns to build from."));
                }
                columns.reserve(d.len());
                for (k, val) in d.iter() {
                    // A COLUMN NAME IS A STRING. Every other key kind is refused by name
                    // rather than stringified: `1` and `"1"` would become the same column,
                    // and silently merging two columns is a wrong answer.
                    let crate::value::DictKey::Str(cname) = k else {
                        return Err(HelixError::new(
                            format!(
                                "`dataframe` needs string column names, but this dict has a {} key",
                                match k {
                                    crate::value::DictKey::Int(_) => "Int",
                                    crate::value::DictKey::Bool(_) => "Bool",
                                    crate::value::DictKey::Dna(_) => "Dna",
                                    crate::value::DictKey::Str(_) => "String",
                                }
                            ),
                            line,
                            col,
                        )
                        .hint("build the dict with string keys: `dict().insert(\"age\", [30, 41])`."));
                    };
                    columns.push((
                        cname.to_string(),
                        array_to_coldata(cname, val, line, col)?,
                    ));
                }
            }
            other => {
                return Err(type_err("dataframe", "a record or dict of columns", other, line, col)
                    .hint(
                        "e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`, or a dict of \
                         string names to columns when the schema is only known at run time.",
                    ))
            }
        }
        Ok(Value::dataframe(crate::backend::build_frame(columns, line, col)?))
    
}
