//! Reading values out of collections: indexing (`eval_index`), slicing
//! (`eval_slice`), record/tuple field access (`eval_field`), and destructuring
//! (`destructure_parts`/`pattern_parts`). Also the no-arg DataFrame value-methods
//! (`df_value_method`: count/columns/head/cache). Shared by both engines.

use super::*;
use std::rc::Rc;

use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::tensor;
use crate::value::Value;

/// Resolve a Python-style slice into the concrete element indices to take.
fn slice_indices(len: i64, start: Option<i64>, stop: Option<i64>, step: i64) -> Vec<usize> {
    let (lower, upper) = if step < 0 { (-1i64, len - 1) } else { (0i64, len) };
    let clamp = |x: i64| -> i64 {
        let mut v = x;
        if v < 0 {
            // saturating so an extreme negative bound can't overflow i64
            v = v.saturating_add(len);
            if v < lower {
                v = lower;
            }
        } else if v > upper {
            v = upper;
        }
        v
    };
    let start = match start {
        Some(s) => clamp(s),
        None => if step < 0 { upper } else { lower },
    };
    let stop = match stop {
        Some(s) => clamp(s),
        None => if step < 0 { lower } else { upper },
    };
    let mut out = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            out.push(i as usize);
            i += step;
        }
    } else {
        while i > stop {
            out.push(i as usize);
            i += step;
        }
    }
    out
}

pub(crate) fn eval_slice(
    recv: &Value,
    start: Option<i64>,
    stop: Option<i64>,
    step: i64,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match recv {
        Value::Array(items) => {
            let idxs = slice_indices(items.len() as i64, start, stop, step);
            let out: Vec<Value> = idxs.iter().map(|&i| items.get(i)).collect();
            Ok(Value::array(out))
        }
        Value::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let idxs = slice_indices(chars.len() as i64, start, stop, step);
            let out: String = idxs.iter().map(|&i| chars[i]).collect();
            Ok(Value::Str(Rc::new(out)))
        }
        Value::Dna(s) => {
            // ASCII (uppercase ACGT): index bytes directly, no intermediate `Vec<char>`.
            let bytes = s.as_bytes();
            let idxs = slice_indices(bytes.len() as i64, start, stop, step);
            let out: String = idxs.iter().map(|&i| bytes[i] as char).collect();
            Ok(Value::Dna(Rc::new(out)))
        }
        Value::Tensor(t) => {
            if t.ndim() == 0 {
                return Err(HelixError::new("cannot slice a 0-D (scalar) tensor", line, col));
            }
            let idxs = slice_indices(t.shape()[0] as i64, start, stop, step);
            Ok(tensor::slice_first(t, &idxs))
        }
        // A Python handle slices via its own `__getitem__` (numpy/list semantics).
        Value::PyObject(h) => crate::python::slice(h, start, stop, step, line, col),
        Value::Missing => Ok(Value::Missing),
        other => Err(HelixError::new(
            format!("a value of type {} cannot be sliced", other.type_name()),
            line,
            col,
        )
        .hint("slicing works on arrays, strings, DNA, and tensors (first axis).")),
    }
}

/// Unpack a tuple/array into exactly `n` values for destructuring (shared by
/// both engines). Errors if the value isn't a tuple/array, or the arity is wrong.
pub(crate) fn destructure_parts(
    v: &Value,
    n: usize,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    let parts = match v {
        Value::Tuple(t) => (**t).clone(),
        Value::Array(a) => a.to_values().into_owned(),
        other => {
            return Err(HelixError::new(
                format!(
                    "cannot destructure a value of type {} into {} names",
                    other.type_name(),
                    n
                ),
                line,
                col,
            )
            .hint("the right-hand side must be a tuple or array, e.g. `a, b = (1, 2)`."))
        }
    };
    if parts.len() != n {
        return Err(HelixError::new(
            format!("cannot destructure {} values into {} names", parts.len(), n),
            line,
            col,
        ));
    }
    Ok(parts)
}

/// Split a comprehension element into `n` parts for a multi-binder pattern
/// (`xs.map((a, b) => ...)`). Distinct from [`destructure_parts`] (the `a, b = …`
/// statement form) in its wording — "parameters" / "lambda expects N values". The
/// single source of truth for both the tree-walker ([`Interp::eval_with_pattern`])
/// and the VM (`Op::DestructureBind`), so the two engines never diverge here.
pub(crate) fn pattern_parts(
    v: &Value,
    n: usize,
    line: usize,
    col: usize,
) -> Result<Vec<Value>, HelixError> {
    let parts = match v {
        Value::Tuple(t) => (**t).clone(),
        Value::Array(a) => a.to_values().into_owned(),
        other => {
            return Err(HelixError::new(
                format!(
                    "cannot destructure a value of type {} into {} parameters",
                    other.type_name(),
                    n
                ),
                line,
                col,
            )
            .hint("the element must be a tuple or array (e.g. from `zip`/`enumerate`)."))
        }
    };
    if parts.len() != n {
        return Err(HelixError::new(
            format!("lambda expects {} values, but the element has {}", n, parts.len()),
            line,
            col,
        ));
    }
    Ok(parts)
}

/// DataFrame methods whose arguments are plain *values* (not column refs), so
/// the VM can dispatch them after evaluating args. The column-argument verbs
/// (`where`/`select`/`sort`/`group`) are not here — they take unevaluated ASTs
/// and remain on the tree-walker. Mirrors the matching arms of `eval_df_method`.
/// The single column-name argument of `df.column("age")` — an evaluated string.
/// Shared by both engines (the tree-walker evaluates the AST arg first).
pub(crate) fn column_arg(args: &[Value], line: usize, col: usize) -> Result<String, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new("`column` takes one column name", line, col)
            .hint("e.g. `df.column(\"age\")`."));
    }
    match &args[0] {
        Value::Str(s) => Ok((**s).clone()),
        other => Err(type_err("column", "a column name string", other, line, col)),
    }
}

/// Extract zero or more column-name strings from evaluated args (for `unique`).
pub(crate) fn column_args(
    who: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Vec<String>, HelixError> {
    args.iter()
        .map(|v| match v {
            Value::Str(s) => Ok((**s).clone()),
            other => Err(type_err(who, "a column name string", other, line, col)),
        })
        .collect()
}

pub(crate) fn df_value_method(
    lf: &Df,
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "count" => {
            if !args.is_empty() {
                return Err(HelixError::new("`count` takes no arguments", line, col));
            }
            Ok(Value::Int(lf.row_count(line, col)? as i64))
        }
        "columns" => {
            if !args.is_empty() {
                return Err(HelixError::new("`columns` takes no arguments", line, col));
            }
            let names: Vec<Value> = lf
                .column_names(line, col)?
                .into_iter()
                .map(|c| Value::Str(Rc::new(c)))
                .collect();
            Ok(Value::array(names))
        }
        "cache" => {
            if !args.is_empty() {
                return Err(HelixError::new("`cache` takes no arguments", line, col)
                    .hint("e.g. `big = read_csv(\"x.csv\").cache()` to reuse without re-scanning."));
            }
            Ok(Value::dataframe(lf.cache(line, col)?))
        }
        "head" => {
            if args.len() != 1 {
                return Err(HelixError::new("`head` takes a row count", line, col)
                    .hint("e.g. `df.head(5)`."));
            }
            let n = as_int(&args[0], "head", line, col)?.max(0) as usize;
            Ok(Value::dataframe(lf.head(n)))
        }
        "vstack" => {
            if args.len() != 1 {
                return Err(HelixError::new("`vstack` takes one DataFrame to append", line, col)
                    .hint("e.g. `kb.vstack(new_rows)` to append rows."));
            }
            match &args[0] {
                Value::DataFrame(bottom) => Ok(Value::dataframe(lf.vstack(bottom, line, col)?)),
                v => Err(HelixError::new(
                    format!("`vstack` expects a DataFrame, found {}", v.type_name()),
                    line,
                    col,
                )),
            }
        }
        "unique" => {
            // `unique()` drops duplicate whole rows; `unique("k1", "k2")` keeps one
            // row per key combination (newest wins — upsert).
            let names = column_args("unique", &args, line, col)?;
            if !names.is_empty() {
                crate::backend::validate_columns_exist(lf, &names, line, col)?;
            }
            Ok(Value::dataframe(lf.unique_by(&names, line, col)?))
        }
        "column" => {
            let name = column_arg(&args, line, col)?;
            Ok(Value::array_sniff(lf.column_values(&name, line, col)?))
        }
        "to_json" => {
            if !args.is_empty() {
                return Err(HelixError::new("`to_json` takes no arguments", line, col));
            }
            crate::writers::to_json(&[Value::dataframe(lf.clone())], line, col)
        }
        "write_csv" | "write_tsv" | "write_json" | "write_parquet" | "to_html" | "to_markdown"
        | "to_table" => {
            crate::interp::export_method(Value::dataframe(lf.clone()), name, &args, line, col)
        }
        _ => {
            let methods = crate::registry::methods_of(crate::registry::DF_METHODS);
            let mut err =
                HelixError::new(format!("a DataFrame has no method `{}`", name), line, col);
            if let Some(s) = suggest(name, &methods) {
                err = err.hint(format!("did you mean `{}`?", s));
            } else {
                err = err.hint(format!("DataFrame methods: {}", methods.join(", ")));
            }
            Err(err)
        }
    }
}

/// Record field access `r.name`. Shared by the tree-walker and the VM; the field
/// name arrives pre-interned (the VM from its `GetField` op, the tree-walker by
/// interning the static AST name), so the lookup compares a single `u32` per key.
pub(crate) fn eval_field(r: &Value, name: Symbol, line: usize, col: usize) -> Result<Value, HelixError> {
    match r {
        Value::Record(fields) => fields
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                let mut err =
                    HelixError::new(format!("record has no field `{}`", name), line, col);
                if let Some(s) = suggest(name.as_str(), &keys) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("fields: {}", keys.join(", ")));
                }
                err
            }),
        Value::Missing => Ok(Value::Missing), // propagate
        Value::PyObject(h) => crate::python::getattr(h, name.as_str(), line, col),
        other => Err(HelixError::new(
            format!("a value of type {} has no field `{}`", other.type_name(), name),
            line,
            col,
        )
        .hint("field access `x.name` works on records; methods need `()`.")),
    }
}

/// Resolve one slice bound value to an optional index (shared by both engines):
/// an `Int` is the bound, `missing` means omitted, anything else is an error.
pub(crate) fn slice_bound(v: &Value, line: usize, col: usize) -> Result<Option<i64>, HelixError> {
    match v {
        Value::Int(i) => Ok(Some(*i)),
        Value::Missing => Ok(None),
        other => Err(type_err("slice bound", "an integer", other, line, col)),
    }
}

pub(crate) fn eval_index(recv: &Value, idx: &Value, line: usize, col: usize) -> Result<Value, HelixError> {
    // A Python handle forwards `[...]` to its `__getitem__` (numpy `a[i]`, dict `d[k]`).
    if let Value::PyObject(h) = recv {
        return crate::python::index(h, idx, line, col);
    }
    // `r["key"]` — dynamic record-field access (handy for JSON whose keys aren't
    // valid identifiers). An absent key yields `missing`: this is the safe/optional
    // accessor, while `.field` is the static one that errors on a typo.
    if let (Value::Record(fields), Value::Str(key)) = (recv, idx) {
        // `lookup` (never inserts) so a dynamic miss-key in a loop can't grow the
        // interner; an un-interned key matches no field → `missing`, as before.
        return Ok(Symbol::lookup(key.as_str())
            .and_then(|want| fields.iter().find(|(k, _)| *k == want))
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Missing));
    }
    let i = match idx {
        Value::Int(i) => *i,
        other => return Err(type_err("index", "an integer", other, line, col)),
    };
    match recv {
        Value::Array(items) => {
            let n = items.len() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                )
                .hint("valid indices run from 0 to length-1; negative indices count from the end."));
            }
            Ok(items.get(real as usize))
        }
        Value::Tuple(items) => {
            let n = items.len() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                )
                .hint("valid indices run from 0 to length-1; negative indices count from the end."));
            }
            Ok(items[real as usize].clone())
        }
        Value::Dna(s) => {
            // DNA is ASCII (uppercase ACGT), so byte length is the char count and a
            // byte index *is* the char — O(1), no per-access `Vec<char>` allocation
            // (which made indexing in a loop O(n²)). Yields a one-char `Str`, as before.
            let n = s.len() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                ));
            }
            Ok(Value::Str(Rc::new((s.as_bytes()[real as usize] as char).to_string())))
        }
        Value::Str(s) => {
            // General UTF-8: count then fetch by char index without materializing a
            // `Vec<char>`, so indexing in a loop is O(n) per access, not O(n) + an
            // allocation.
            let n = s.chars().count() as i64;
            let real = if i < 0 { n + i } else { i };
            if real < 0 || real >= n {
                return Err(HelixError::new(
                    format!("index {} is out of bounds for length {}", i, n),
                    line,
                    col,
                ));
            }
            let ch = s.chars().nth(real as usize).expect("index bounds-checked above");
            Ok(Value::Str(Rc::new(ch.to_string())))
        }
        Value::Tensor(t) => tensor::index_first(t, i, line, col),
        other => Err(HelixError::new(
            format!("a value of type {} cannot be indexed", other.type_name()),
            line,
            col,
        )),
    }
}
