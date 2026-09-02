//! Reading values out of collections: indexing (`eval_index`), slicing
//! (`eval_slice`), record/tuple field access (`eval_field`), and destructuring
//! (`destructure_parts`/`pattern_parts`). Also the DataFrame value-methods
//! (`df_value_method`: count/columns/cache/head/vstack/unique/column/to_json plus the
//! write_*/to_html/to_markdown/to_table export verbs). Shared by both engines.

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
    // The cursor advance is CHECKED: `step` is raw user input bounded only by != 0, so
    // `1 + i64::MAX` must END the slice, not wrap the cursor — the wrapped value's
    // `as usize` became a 2^63 index and aborted the process on `items.get(...)`
    // (found by the stability sweep: `[10,20,30,40,50][1::9223372036854775807]`).
    // Overflow can only happen PAST the last in-range index, so breaking is exact.
    if step > 0 {
        while i < stop {
            out.push(i as usize);
            match i.checked_add(step) {
                Some(n) => i = n,
                None => break,
            }
        }
    } else {
        while i > stop {
            out.push(i as usize);
            match i.checked_add(step) {
                Some(n) => i = n,
                None => break,
            }
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
        // A tracked tensor slices like a plain one — resolved by the SAME
        // `slice_indices`, so every edge (negative bounds, a reversing step, an
        // empty range) lands identically — and the rows stay on the tape.
        // Unguarded, so a tracked SCALAR reaches `autodiff::slice` and is refused
        // in the plain path's words ("cannot slice a 0-D (scalar) tensor") rather
        // than falling through to the generic arm and leaking the name `Node`.
        // `axis0_len` is 0 there and the resolved index list is empty, which the
        // rank check rejects before it is ever read.
        Value::Node(n) => {
            let idxs = slice_indices(n.axis0_len() as i64, start, stop, step);
            crate::autodiff::slice(n, &idxs, line, col)
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
/// single source of truth for both the tree-walker (`Interp::eval_pattern_loop`)
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

/// DataFrame methods whose arguments are plain *values* (not column refs), so
/// the VM can dispatch them after evaluating args. The column-argument verbs
/// (`where`/`select`/`sort`/`group`) are not here — they take unevaluated ASTs
/// and remain on the tree-walker. Mirrors the matching arms of `eval_df_method`.
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
        "head" | "tail" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("`{name}` takes a row count"), line, col)
                    .hint("e.g. `df.head(5)`."));
            }
            let n = as_int(&args[0], name, line, col)?.max(0) as usize;
            Ok(Value::dataframe(if name == "head" { lf.head(n) } else { lf.tail(n) }))
        }
        // A ROW WINDOW, which a chunked store needs and `head` alone cannot express: to cut
        // a sorted frame into chunks you must be able to start somewhere other than row 0.
        // Reported from the field as a blocker for exactly that.
        "slice" => {
            if args.len() != 2 {
                return Err(HelixError::new("`slice` takes an offset and a row count", line, col)
                    .hint("e.g. `df.slice(1000, 500)` for rows 1000..1500."));
            }
            let offset = as_int(&args[0], "slice", line, col)?;
            let len = as_int(&args[1], "slice", line, col)?;
            if offset < 0 || len < 0 {
                return Err(HelixError::new(
                    format!("`slice` needs a non-negative offset and count, got {offset} and {len}"),
                    line,
                    col,
                )
                .hint("counting back from the end is `tail(n)`."));
            }
            // CLAMPED, NOT REFUSED: an offset past the end is an empty frame and a count
            // past the end is short. That is how the last chunk of a scan reads, and
            // erroring would make the caller compute the row count first — which on a lazy
            // frame means materializing it.
            Ok(Value::dataframe(lf.slice(offset as usize, len as usize)))
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
        // Two NAME arguments, both ordinary evaluated strings -- so a library can pass its
        // own parameters through without any of ADR 0028's name-position machinery, which
        // is exactly what a generic relation-attach needs.
        "rename" => {
            if args.len() != 2 {
                return Err(HelixError::new(
                    "`rename` takes the old column name and the new one",
                    line,
                    col,
                )
                .hint("e.g. `df.rename(\"author_id\", \"id\")`."));
            }
            let names = column_args("rename", &args, line, col)?;
            Ok(Value::dataframe(lf.rename(&names[0], &names[1], line, col)?))
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
        // A COLUMN VERB REACHING HERE MEANS ITS ARGUMENTS WERE ALREADY EVALUATED, which
        // happens when the receiver's type was not known statically — inside
        // `fn f(d, k) = d.sort(k)`, for instance. These verbs take UNEVALUATED asts because
        // their arguments are column references rather than values, so they are deliberately
        // absent from this table.
        //
        // Falling through to the catch-all produced "a DataFrame has no method `sort` — did
        // you mean `sort`?": a message that contradicts itself and sends the reader looking
        // for a typo that is not there. The method exists; it cannot be reached THIS way.
        "where" | "filter" | "select" | "sort" | "group" | "with" => Err(HelixError::new(
            format!("`{name}` needs its columns named at the call site, not passed as values"),
            line,
            col,
        )
        .hint(
            "write the column with the `@` sigil — `df.sort(@price)`. A column name held in a \
             VARIABLE is not supported: `df.sort(k)` reads `k` as a column literally named \
             \"k\". To order by a name chosen at run time, build the permutation yourself \
             (`range(0, n).sort_by(...)`) and rebuild with `dataframe(dict)`.",
        )),
        _ => {
            let methods = crate::registry::methods_of(crate::registry::DF_METHODS);
            let err =
                HelixError::new(
                    format!("{} has no method `{}`", crate::value::with_article("DataFrame"), name),
                    line,
                    col,
                );
            Err(match crate::suggest::hint(name, crate::suggest::Site::Method, &methods) {
                Some(h) => err.hint(h),
                None => err.hint(format!("DataFrame methods: {}", methods.join(", "))),
            })
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
                    // Canonical order, matching how the record itself prints.
                    let mut keys = keys;
                    keys.sort_unstable();
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
    // `d[key]` — dict lookup (ADR 0020); an absent key yields `missing`, the same safe
    // accessor as `.get(key)`, so `d[k] ?? default` works.
    // Headers index like a dict — string key, missing on absence — with the lookup
    // case-insensitive, because that is the type's contract (RFC 9110 field names).
    if let (Value::Headers(pairs), Value::Str(key)) = (recv, idx) {
        return Ok(pairs
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(key))
            .map(|(_, v)| Value::Str(std::rc::Rc::new(v.clone())))
            .unwrap_or(Value::Missing));
    }
    if let Value::Dict(map) = recv {
        let key = crate::value::DictKey::from_value(idx).map_err(|m| HelixError::new(m, line, col))?;
        return Ok(map.map().get(&key).cloned().unwrap_or(Value::Missing));
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
        // A tracked tensor indexes exactly as a plain one does — same bounds, same
        // wording — except the element stays on the tape, so `W[0][1]` is a
        // differentiable read of one weight rather than a dead end.
        Value::Node(n) => crate::autodiff::index(n, i, line, col),
        other => {
            let err = HelixError::new(
                format!("a value of type {} cannot be indexed", other.type_name()),
                line,
                col,
            );
            // `df[0]` is the single most common thing a pandas user types, and it was the
            // largest no-help shape in the adversarial sweep. A frame is columnar and lazy —
            // there is no row to hand back — so name the two verbs that do what was meant.
            Err(match other {
                Value::DataFrame(_) => err.hint(
                    "a DataFrame is columnar and lazy — take rows with `df.head(n)` and a column with `df.column(\"name\")`.",
                ),
                Value::Record(_) => {
                    err.hint("a record is indexed by FIELD NAME: `r.name`, not `r[0]`.")
                }
                _ => err,
            })
        }
    }
}
