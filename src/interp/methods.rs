//! Value-method dispatch (`call_method`) and the per-type method implementations
//! for arrays, strings, and DNA, plus the shared numeric helpers (Neumaier
//! compensated summation, population standard deviation). These are free
//! functions shared by both the tree-walker and the bytecode VM — the parent
//! module re-exports them, so `crate::interp::call_method` still resolves.

use super::*;
use std::rc::Rc;

use crate::error::{suggest, HelixError};
use crate::value::Value;


/// GC fraction of a DNA string (`N` excluded from the denominator), erroring on an
/// empty sequence — shared by the `at_content` and `mean_gc` methods.
fn dna_gc(s: &str, who: &str, line: usize, col: usize) -> Result<f64, HelixError> {
    if s.is_empty() {
        return Err(HelixError::new(
            format!("cannot compute `{who}` of an empty sequence"),
            line,
            col,
        ));
    }
    let gc = s.chars().filter(|c| *c == 'G' || *c == 'C').count();
    let called = s.chars().filter(|c| *c != 'N').count();
    Ok(if called == 0 { 0.0 } else { gc as f64 / called as f64 })
}

/// Prepend the receiver and call the matching chart/writer/export free function.
/// Shared by the Array and DataFrame method handlers so the two engines and both
/// receiver types stay byte-for-byte in lockstep (the differential oracle's only
/// risk here). The underlying `writers::*`/`chart::*` already accept the data as
/// the first positional argument.
pub(crate) fn export_method(
    recv: Value,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // Capability gate (ADR 0021): the `write_*` exports are `FsWrite` authority. This is the
    // shared sink for both engines and both receiver types, so one gate here covers them all.
    // (`to_html`/`to_markdown`/`to_table`/charts return strings — `Pure`, ungated.)
    crate::capability::gate_method(name, args, line, col)?;
    // `write_parquet` is DataFrame-only and goes straight to the backend.
    if name == "write_parquet" {
        return match (&recv, args.first()) {
            (Value::DataFrame(df), Some(Value::Str(p))) => {
                df.write_parquet(p, line, col)?;
                Ok(Value::Unit)
            }
            _ => Err(HelixError::new("`write_parquet` needs a string path", line, col)),
        };
    }
    let mut a = Vec::with_capacity(args.len() + 1);
    a.push(recv);
    a.extend_from_slice(args);
    match name {
        "bar_chart" => crate::chart::bar(&a, line, col),
        "histogram" => crate::chart::hist(&a, line, col),
        "line_chart" => crate::chart::line(&a, line, col),
        "sparkline" => crate::chart::sparkline(&a, line, col),
        "scatter" => crate::chart::scatter(&a, line, col),
        "svg_bar" => crate::writers::svg_bar(&a, line, col),
        "svg_line" => crate::writers::svg_line(&a, line, col),
        "write_csv" => crate::writers::write_csv(&a, line, col),
        "write_tsv" => crate::writers::write_tsv(&a, line, col),
        "write_json" => crate::writers::write_json(&a, line, col),
        "to_html" => crate::writers::to_html(&a, line, col),
        "to_markdown" => crate::writers::to_markdown(&a, line, col),
        "to_table" => crate::writers::to_table(&a, line, col),
        "write_fasta" => crate::writers::write_fasta(&a, line, col),
        "write_fastq" => crate::writers::write_fastq(&a, line, col),
        other => Err(HelixError::new(format!("no export method `{other}`"), line, col)),
    }
}

/// Methods on a [`Value::Net`] handle — the HTTP server surface (`src/serve.rs`).
/// `accept` (on a listener) blocks for one request and returns `(request, connection)`;
/// `respond` (on a connection) writes the reply. Both are effects, outside the oracle.
fn net_method(
    h: &Rc<crate::serve::NetHandle>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // Capability gate (ADR 0021): the socket-touching verbs (accept/poll/respond/sse/send)
    // are `Net` authority — defense-in-depth behind `listen` (itself gated). `request` reads
    // the already-parsed request record (no socket I/O) and is ungated (`Pure`).
    crate::capability::gate_method(name, args, line, col)?;
    match name {
        "accept" => {
            if !args.is_empty() {
                return Err(HelixError::new("`accept` takes no arguments", line, col));
            }
            crate::serve::accept(h, line, col)
        }
        "poll" => {
            if !args.is_empty() {
                return Err(HelixError::new("`poll` takes no arguments", line, col));
            }
            crate::serve::poll(h, line, col)
        }
        // Cooperative event-loop server: non-blocking accept + per-connection non-blocking
        // read, so one thread serves many keep-alive connections interleaved.
        "accept_poll" => {
            if !args.is_empty() {
                return Err(HelixError::new("`accept_poll` takes no arguments", line, col));
            }
            crate::serve::accept_poll(h, line, col)
        }
        "poll_request" => {
            if !args.is_empty() {
                return Err(HelixError::new("`poll_request` takes no arguments", line, col));
            }
            crate::serve::poll_request(h, line, col)
        }
        "is_open" => {
            if !args.is_empty() {
                return Err(HelixError::new("`is_open` takes no arguments", line, col));
            }
            crate::serve::is_open(h, line, col)
        }
        "wait" => {
            if args.len() != 2 {
                return Err(HelixError::new(
                    "`wait` takes (conns, timeout_ms)",
                    line,
                    col,
                )
                .hint("e.g. `l.wait(conns, 50)` — block until a connection is ready."));
            }
            let timeout = match &args[1] {
                Value::Int(n) => *n,
                other => return Err(type_err("wait", "a timeout in ms (integer)", other, line, col)),
            };
            crate::serve::wait(h, &args[0], timeout, line, col)
        }
        "request" => {
            if !args.is_empty() {
                return Err(HelixError::new("`request` takes no arguments", line, col));
            }
            crate::serve::request(h, line, col)
        }
        "respond" => {
            if args.len() != 1 {
                return Err(HelixError::new("`respond` takes one response value", line, col)
                    .hint("e.g. `conn.respond({ status: 200, json: data })`."));
            }
            crate::serve::respond(h, &args[0], line, col)
        }
        "sse" => {
            if !args.is_empty() {
                return Err(HelixError::new("`sse` takes no arguments", line, col));
            }
            crate::serve::sse(h, line, col)
        }
        "send" => {
            if args.len() != 1 {
                return Err(HelixError::new("`send` takes one event value", line, col)
                    .hint("e.g. `conn.send(world.to_json())` — returns false when the client leaves."));
            }
            crate::serve::send(h, &args[0], line, col)
        }
        // Streaming client (`http_stream`): pull chunks and read the status.
        "next" => {
            if !args.is_empty() {
                return Err(HelixError::new("`next` takes no arguments", line, col));
            }
            crate::serve::stream_next(h, line, col)
        }
        "status" => {
            if !args.is_empty() {
                return Err(HelixError::new("`status` takes no arguments", line, col));
            }
            crate::serve::stream_status(h, line, col)
        }
        "close" => {
            if !args.is_empty() {
                return Err(HelixError::new("`close` takes no arguments", line, col));
            }
            crate::serve::stream_close(h, line, col)
        }
        other => Err(HelixError::new(format!("type Net has no method `{other}`"), line, col)
            .hint("a listener has `accept`/`poll`; a connection has `request`/`respond`, or `sse`/`send` to stream; an http_stream has `status`/`next`/`close`.")),
    }
}

pub(crate) fn call_method(
    recv: &Value,
    name: &str,
    args: Vec<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // `is_missing` is universal: true only for the `missing` value itself.
    if name == "is_missing" {
        if !args.is_empty() {
            return Err(HelixError::new("`is_missing` takes no arguments", line, col));
        }
        return Ok(Value::Bool(matches!(recv, Value::Missing)));
    }
    // `to_json` is universal too — it serializes any value (and `missing` → `null`,
    // the JSON convention), so it runs before the missing-propagation rule below.
    // (DataFrame/GroupBy receivers are intercepted earlier and never reach here.)
    if name == "to_json" {
        if !args.is_empty() {
            return Err(HelixError::new("`to_json` takes no arguments", line, col));
        }
        return crate::writers::to_json(std::slice::from_ref(recv), line, col);
    }
    // `missing` propagates through method calls just as it does through field/index
    // access (ADR 0001's three-valued model): any method on `missing` yields `missing`
    // — so `read.qual.phred().mean()` on a quality-less read is `missing`, not an error.
    // `is_missing` (above) is the sole exception.
    if matches!(recv, Value::Missing) {
        return Ok(Value::Missing);
    }
    // If a method argument is tracked (autodiff) but the receiver is a plain number
    // or tensor, lift the receiver into the graph too — so `X.matmul(w)` differentiates
    // through `w` even though `X` is a constant.
    if !matches!(recv, Value::Node(_))
        && args.iter().any(|a| matches!(a, Value::Node(_)))
        && let Some(n) = crate::autodiff::lift(recv)
    {
        return crate::autodiff::method(&n, name, &args, line, col);
    }
    match recv {
        Value::Array(items) => match array_numeric_fast(items, name, &args, line, col)? {
            // A typed array's numeric reduction reads the packed buffer directly.
            Some(v) => Ok(v),
            // Everything else materializes to `Value`s and runs the general path.
            None => array_method(&items.to_values(), name, &args, line, col),
        },
        Value::Str(s) => string_method(s, name, &args, line, col),
        Value::Dna(s) => dna_method(s, name, &args, line, col),
        Value::Node(n) => crate::autodiff::method(n, name, &args, line, col),
        Value::Tensor(t) => crate::tensor::method(t, name, &args, line, col),
        Value::PyObject(h) => crate::python::method(h, name, &args, line, col),
        Value::Dict(map) => dict_method(map, name, &args, line, col),
        Value::Net(h) => net_method(h, name, &args, line, col),
        Value::Record(fields) => record_method(fields, name, &args, line, col),
        other => Err(HelixError::new(
            format!("a {} has no method `{}`", other.type_name(), name),
            line,
            col,
        )),
    }
}

/// Methods on a [`Value::Dict`] (ADR 0020). Lookups (`get`/`contains`) are O(log n);
/// enumeration (`keys`/`values`/`items`) is sorted by key, so output is deterministic.
/// `insert`/`remove` are immutable — they return a new dict (the map is cloned, so a
/// one-shot update is O(n); build in bulk with `pairs.to_dict()` for O(n log n)).
fn dict_method(
    map: &Rc<std::collections::BTreeMap<crate::value::DictKey, Value>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    use crate::value::DictKey;
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    n,
                    if n == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ))
        }
    };
    let key_of = |v: &Value| DictKey::from_value(v).map_err(|m| HelixError::new(m, line, col));
    match name {
        // `get(k)` → the value, or `missing` when absent (so `d.get(k) ?? default` works).
        "get" => {
            arity(1)?;
            Ok(map.get(&key_of(&args[0])?).cloned().unwrap_or(Value::Missing))
        }
        // `has` is the alias that matches a record's `has` — the same key-presence question,
        // one name across both keyed types.
        "contains" | "has" => {
            arity(1)?;
            Ok(Value::Bool(map.contains_key(&key_of(&args[0])?)))
        }
        "count" | "length" => {
            arity(0)?;
            Ok(Value::Int(map.len() as i64))
        }
        "keys" => {
            arity(0)?;
            Ok(Value::array(map.keys().map(|k| k.to_value()).collect()))
        }
        "values" => {
            arity(0)?;
            Ok(Value::array(map.values().cloned().collect()))
        }
        // `(key, value)` tuples, sorted by key — round-trips through `to_dict`.
        "items" => {
            arity(0)?;
            Ok(Value::array(
                map.iter()
                    .map(|(k, v)| Value::Tuple(Rc::new(vec![k.to_value(), v.clone()])))
                    .collect(),
            ))
        }
        "insert" => {
            arity(2)?;
            let k = key_of(&args[0])?;
            let mut new = (**map).clone();
            new.insert(k, args[1].clone());
            Ok(Value::Dict(Rc::new(new)))
        }
        "remove" => {
            arity(1)?;
            let k = key_of(&args[0])?;
            let mut new = (**map).clone();
            new.remove(&k);
            Ok(Value::Dict(Rc::new(new)))
        }
        _ => Err(unknown_method(
            "Dict",
            name,
            &crate::registry::methods_of(crate::registry::DICT_METHODS),
            line,
            col,
        )),
    }
}

/// Methods on a [`Value::Record`] for **dynamic** field access — the escape hatch for
/// consuming unknown-shape data (a parsed JSON API response). `get`/`has`/`keys` look fields
/// up by string name at runtime, so a maybe-absent field is `missing`/`false` rather than a
/// compile error. Static `rec.field` access is unchanged; this is for shapes you don't know
/// until runtime.
fn record_method(
    fields: &Rc<Vec<(crate::symbol::Symbol, Value)>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{name}` expects {n} argument{}, got {}", if n == 1 { "" } else { "s" }, args.len()),
                line,
                col,
            ))
        }
    };
    let key = |v: &Value| -> Result<String, HelixError> {
        match v {
            Value::Str(s) => Ok((**s).clone()),
            other => Err(type_err(name, "a string field name", other, line, col)),
        }
    };
    match name {
        // `get(k)` → the field's value, or `missing` when absent (so `rec.get(k) ?? default`
        // works). `get(k, default)` → the value, or `default` when absent.
        "get" => {
            if args.len() != 1 && args.len() != 2 {
                return Err(HelixError::new(
                    format!("`get` expects 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                ));
            }
            let k = key(&args[0])?;
            let found = fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v.clone());
            Ok(found.unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Missing)))
        }
        "has" => {
            arity(1)?;
            let k = key(&args[0])?;
            Ok(Value::Bool(fields.iter().any(|(s, _)| s.as_str() == k)))
        }
        "keys" => {
            arity(0)?;
            Ok(Value::array(
                fields.iter().map(|(s, _)| Value::Str(Rc::new(s.as_str().to_string()))).collect(),
            ))
        }
        "values" => {
            arity(0)?;
            Ok(Value::array(fields.iter().map(|(_, v)| v.clone()).collect()))
        }
        "items" => {
            arity(0)?;
            Ok(Value::array(
                fields
                    .iter()
                    .map(|(s, v)| {
                        Value::Tuple(Rc::new(vec![
                            Value::Str(Rc::new(s.as_str().to_string())),
                            v.clone(),
                        ]))
                    })
                    .collect(),
            ))
        }
        _ => Err(unknown_method(
            "Record",
            name,
            &crate::registry::methods_of(crate::registry::RECORD_METHODS),
            line,
            col,
        )),
    }
}

fn numeric_vec(items: &[Value], who: &str, line: usize, col: usize) -> Result<Vec<f64>, HelixError> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        match v.as_f64() {
            Some(x) => out.push(x),
            None => {
                return Err(HelixError::new(
                    format!(
                        "`{}` needs an array of numbers, but element {} is a {}",
                        who,
                        i,
                        v.type_name()
                    ),
                    line,
                    col,
                ))
            }
        }
    }
    Ok(out)
}

/// True if any element is `missing` *or* a `NaN` float — every numeric aggregation
/// propagates both as `missing` (ADR-0001). `NaN` is "not a number" and, being
/// unordered, would otherwise silently corrupt sort-based stats (a stray `NaN`
/// lands at an arbitrary position, giving a wrong median/quantile). `inf` is left
/// alone: it orders correctly and yields a well-defined (if extreme) result.
fn missing_or_nan(items: &[Value]) -> bool {
    items
        .iter()
        .any(|v| matches!(v, Value::Missing) || matches!(v, Value::Float(f) if f.is_nan()))
}

/// Order two numeric `Value`s, comparing two `Int`s **exactly** rather than via
/// their `f64` widening. Widening collapses distinct `i64`s above 2^53 to one
/// value, which made the boxed `min`/`max`/`sort` path pick the wrong element and
/// disagree with the exact packed-`Int` path; an `i64`-direct compare keeps them in
/// lock-step. Callers guarantee both values are numeric (`Int`/`Float`).
fn numeric_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        _ => a
            .as_f64()
            .unwrap_or(f64::NAN)
            .partial_cmp(&b.as_f64().unwrap_or(f64::NAN))
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Numeric-reduction fast path for **typed** arrays (`Ints`/`Floats`): read the
/// packed buffer directly, never materializing a `Vec<Value>`. Returns `Ok(None)`
/// for a `Values` array, a non-reduction method, an argument-bearing call, or a
/// `Float` array containing `NaN` — so the caller's general, missing/NaN-aware path
/// runs and the result matches the untyped array exactly. Typed arrays are
/// missing-free by construction, so no missing check is needed here.
fn array_numeric_fast(
    ad: &crate::value::ArrayData,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Option<Value>, HelixError> {
    use crate::value::ArrayData;
    if !matches!(
        name,
        "count" | "sum" | "mean" | "std" | "var" | "median" | "min" | "max"
    ) || !args.is_empty()
    {
        return Ok(None);
    }
    match ad {
        ArrayData::Values(_) => Ok(None),
        ArrayData::Ints(xs) => array_int_reduce(xs, name, line, col).map(Some),
        ArrayData::Floats(xs) => {
            // A `NaN` flips the answer to `missing` under ADR-0001; defer so the
            // general path matches the untyped result exactly.
            if xs.iter().any(|x| x.is_nan()) {
                Ok(None)
            } else {
                array_float_reduce(xs, name, line, col).map(Some)
            }
        }
    }
}

fn array_int_reduce(xs: &[i64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => {
            // i128 accumulate; stay exact `Int` if it fits, else compensated `Float`.
            let wide: i128 = xs.iter().map(|&n| n as i128).sum();
            Ok(match i64::try_from(wide) {
                Ok(n) => Value::Int(n),
                Err(_) => {
                    let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
                    Value::Float(neumaier_sum(&fs))
                }
            })
        }
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            let best = if name == "min" {
                *xs.iter().min().unwrap()
            } else {
                *xs.iter().max().unwrap()
            };
            Ok(Value::Int(best))
        }
        // mean/std/var/median: widen to f64 (still half a `Vec<Value>`).
        _ => {
            let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
            float_stat(&fs, name, line, col)
        }
    }
}

fn array_float_reduce(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => Ok(Value::Float(neumaier_sum(xs))),
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            let mut best = xs[0];
            for &x in &xs[1..] {
                if (name == "min" && x < best) || (name == "max" && x > best) {
                    best = x;
                }
            }
            Ok(Value::Float(best))
        }
        _ => float_stat(xs, name, line, col),
    }
}

/// Shared `f64` reductions (`mean`/`std`/`var`/`median`) — identical kernels to the
/// general `array_method` path, so a typed array's result matches the untyped one.
fn float_stat(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    empty_guard(xs, name, line, col)?;
    Ok(match name {
        "mean" => Value::Float(neumaier_sum(xs) / xs.len() as f64),
        "std" => Value::Float(population_std(xs)),
        "var" => Value::Float(crate::stats::variance(xs)),
        "median" => Value::Float(crate::stats::median(xs)),
        _ => unreachable!("float_stat only handles mean/std/var/median"),
    })
}

fn array_method(
    items: &[Value],
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let no_args = |n: &str| {
        if args.is_empty() {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{}` takes no arguments, got {}", n, args.len()),
                line,
                col,
            ))
        }
    };

    match name {
        "count" | "length" => {
            no_args(name)?;
            // Counts every slot, including `missing` holes. `length` is an alias so the
            // size of an Array/String/Dna is the same call everywhere.
            Ok(Value::Int(items.len() as i64))
        }
        // The first index whose element equals `args[0]` (structural equality), or
        // `missing` if none — pairs with `?? -1` / a `match`. Mirrors `Dna.find`.
        "index_of" => {
            arity("index_of", args, 1, line, col)?;
            match items.iter().position(|v| crate::interp::ops::values_equal(v, &args[0])) {
                Some(i) => Ok(Value::Int(i as i64)),
                None => Ok(Value::Missing),
            }
        }
        "mean" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            Ok(Value::Float(population_std(&xs)))
        }
        "median" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "median", line, col)?;
            empty_guard(&xs, "median", line, col)?;
            Ok(Value::Float(crate::stats::median(&xs)))
        }
        "var" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "var", line, col)?;
            empty_guard(&xs, "var", line, col)?;
            Ok(Value::Float(crate::stats::variance(&xs)))
        }
        "quantile" => {
            // One argument: the probability `p` in [0, 1] (e.g. `xs.quantile(0.95)`).
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`quantile` takes one probability in [0, 1], got {}", args.len()),
                    line,
                    col,
                )
                .hint("e.g. `xs.quantile(0.95)` for the 95th percentile."));
            }
            let p = match args[0].as_f64() {
                Some(p) => p,
                None => return Err(type_err("quantile", "a number in [0, 1]", &args[0], line, col)),
            };
            if !(0.0..=1.0).contains(&p) {
                return Err(HelixError::new(
                    format!("`quantile` needs a probability in [0, 1], got {}", p),
                    line,
                    col,
                )
                .hint("0 is the minimum, 0.5 the median, 1 the maximum."));
            }
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "quantile", line, col)?;
            empty_guard(&xs, "quantile", line, col)?;
            Ok(Value::Float(crate::stats::quantile(&xs, p)))
        }
        "summary" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let mut xs = numeric_vec(items, "summary", line, col)?;
            empty_guard(&xs, "summary", line, col)?;
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A descriptive overview (the `describe()` analogue): count, central
            // tendency, spread, and the three order-statistic extremes/center.
            let fields = vec![
                (Symbol::intern("count"), Value::Int(xs.len() as i64)),
                (Symbol::intern("mean"), Value::Float(crate::stats::mean(&xs))),
                (Symbol::intern("std"), Value::Float(crate::stats::std(&xs))),
                (Symbol::intern("min"), Value::Float(xs[0])),
                (Symbol::intern("median"), Value::Float(crate::stats::quantile_sorted(&xs, 0.5))),
                (Symbol::intern("max"), Value::Float(xs[xs.len() - 1])),
            ];
            Ok(Value::Record(Rc::new(fields)))
        }
        "sum" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            // Keep Int if every element is an Int; otherwise compensated float sum.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                // Accumulate in i128 so a total that exceeds i64 neither panics
                // (debug) nor silently wraps (release): stay exact `Int` when it
                // fits, else promote to a compensated `Float` — mirroring `**`'s
                // Int→Float overflow promotion, so a large sum is never wrong.
                let wide: i128 = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i as i128 } else { 0 })
                    .sum();
                match i64::try_from(wide) {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(_) => {
                        let xs: Vec<f64> = items
                            .iter()
                            .map(|v| if let Value::Int(i) = v { *i as f64 } else { 0.0 })
                            .collect();
                        Ok(Value::Float(neumaier_sum(&xs)))
                    }
                }
            } else {
                let xs = numeric_vec(items, "sum", line, col)?;
                Ok(Value::Float(neumaier_sum(&xs)))
            }
        }
        "min" | "max" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            // `numeric_vec` validates (all-numeric) and powers `empty_guard`, but the
            // selection compares the original `Value`s EXACTLY via `numeric_cmp` — not
            // their f64 widening, which would collapse two i64 above 2^53 to the same
            // value and pick the wrong element (and disagree with the packed Int path).
            let xs = numeric_vec(items, name, line, col)?;
            empty_guard(&xs, name, line, col)?;
            let mut best_idx = 0;
            for i in 1..items.len() {
                let ord = numeric_cmp(&items[i], &items[best_idx]);
                let better = if name == "min" {
                    ord == std::cmp::Ordering::Less
                } else {
                    ord == std::cmp::Ordering::Greater
                };
                if better {
                    best_idx = i;
                }
            }
            Ok(items[best_idx].clone())
        }
        "normalize" => {
            no_args(name)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "normalize", line, col)?;
            empty_guard(&xs, "normalize", line, col)?;
            let mean = neumaier_sum(&xs) / xs.len() as f64;
            let sd = population_std(&xs);
            if sd == 0.0 {
                return Err(HelixError::new(
                    "cannot normalize: all values are identical (standard deviation is 0)",
                    line,
                    col,
                )
                .hint("normalize rescales by spread; a constant column has no spread."));
            }
            let out: Vec<Value> = xs.iter().map(|x| Value::Float((x - mean) / sd)).collect();
            Ok(Value::array(out))
        }
        "drop_missing" => {
            no_args(name)?;
            // Common case: nothing to drop → share the input array (an `Rc` bump,
            // zero allocation) instead of copying every element into a new `Vec`.
            if !items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::array(items.to_vec()));
            }
            let out: Vec<Value> = items
                .iter()
                .filter(|v| !matches!(v, Value::Missing))
                .cloned()
                .collect();
            Ok(Value::array(out))
        }
        "sort" => {
            no_args(name)?;
            let mut sorted: Vec<Value> = items.to_vec();
            // numeric sort if all numeric, else lexical if all strings
            if items.iter().all(|v| v.as_f64().is_some()) {
                // Exact compare (see `numeric_cmp`) so two i64 above 2^53 keep their
                // distinct order instead of collapsing through f64.
                sorted.sort_by(numeric_cmp);
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                return Err(HelixError::new(
                    "`sort` needs an array of all numbers, all strings, or all DNA",
                    line,
                    col,
                ));
            }
            Ok(Value::array(sorted))
        }
        "join" => {
            arity("join", args, 1, line, col)?;
            let sep = str_arg(args, 0, "join", line, col)?;
            // Each element is rendered with its normal display (a string element is
            // its raw text, not a quoted form), then joined: `[1,2,3].join("-")`.
            let joined = items.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(sep);
            Ok(Value::Str(Rc::new(joined)))
        }
        "reverse" => {
            no_args(name)?;
            let mut v: Vec<Value> = items.to_vec();
            v.reverse();
            Ok(Value::array(v))
        }
        "first" | "last" => {
            no_args(name)?;
            // `missing` (not an error) on an empty array, so `xs.first() ?? default` and
            // `is_missing` give a safe first-or-default — missing propagates as elsewhere.
            if items.is_empty() {
                return Ok(Value::Missing);
            }
            let idx = if name == "first" { 0 } else { items.len() - 1 };
            Ok(items[idx].clone())
        }
        "take" => {
            arity("take", args, 1, line, col)?;
            let n = as_int(&args[0], "take", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().take(n).cloned().collect();
            Ok(Value::array(out))
        }
        "drop" => {
            arity("drop", args, 1, line, col)?;
            let n = as_int(&args[0], "drop", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().skip(n).cloned().collect();
            Ok(Value::array(out))
        }
        "zip" => {
            arity("zip", args, 1, line, col)?;
            let other = match &args[0] {
                Value::Array(a) => a.to_values().into_owned(),
                v => {
                    return Err(HelixError::new(
                        format!("`zip` needs an array, but got a {}", v.type_name()),
                        line,
                        col,
                    )
                    .hint("e.g. `xs.zip(ys)` pairs elements positionally."))
                }
            };
            let n = items.len().min(other.len());
            let out: Vec<Value> = (0..n)
                .map(|i| Value::Tuple(Rc::new(vec![items[i].clone(), other[i].clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "enumerate" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "top" => {
            arity("top", args, 1, line, col)?;
            let n = as_int(&args[0], "top", line, col)?.max(0) as usize;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .take(n)
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "frequencies" => {
            // The full value-count histogram as `(value, count)` pairs (count desc,
            // value asc) — `top` without the limit. For k-mer spectra etc.
            no_args(name)?;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "to_dict" => {
            // Array of `(key, value)` pairs → a `Dict` (ADR 0020), for O(log n) lookup
            // instead of an O(n) scan. Later duplicate keys win (last-wins upsert), and
            // `xs.frequencies().to_dict()` turns a histogram into a count lookup.
            no_args(name)?;
            use crate::value::DictKey;
            let mut map = std::collections::BTreeMap::new();
            for (i, item) in items.iter().enumerate() {
                let pair = match item {
                    Value::Tuple(t) if t.len() == 2 => t,
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`to_dict` needs (key, value) pairs, but element {} is a {}",
                                i,
                                other.type_name()
                            ),
                            line,
                            col,
                        )
                        .hint("e.g. `[(\"a\", 1), (\"b\", 2)].to_dict()` or `xs.frequencies().to_dict()`."));
                    }
                };
                let key = DictKey::from_value(&pair[0]).map_err(|m| HelixError::new(m, line, col))?;
                map.insert(key, pair[1].clone());
            }
            Ok(Value::Dict(Rc::new(map)))
        }
        "unique" => {
            // Distinct values in first-seen order. O(n) for string/DNA arrays.
            no_args(name)?;
            let mut out: Vec<Value> = Vec::new();
            if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for v in items.iter() {
                    if seen.insert(v.to_string()) {
                        out.push(v.clone());
                    }
                }
            } else {
                for v in items.iter() {
                    if !out.iter().any(|u| values_equal(u, v)) {
                        out.push(v.clone());
                    }
                }
            }
            Ok(Value::array(out))
        }
        // `xs.concat(a, b, …)` — append the elements of each array argument. The
        // result is re-sniffed so a numeric concat stays a packed (fast) array.
        "concat" => {
            let mut out = items.to_vec();
            for (k, a) in args.iter().enumerate() {
                match a {
                    Value::Array(arr) => out.extend(arr.to_values().iter().cloned()),
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`concat` expects arrays, but argument {} is a {}",
                                k + 1,
                                other.type_name()
                            ),
                            line,
                            col,
                        ))
                    }
                }
            }
            Ok(Value::array_sniff(out))
        }
        // `xss.flatten()` — one level: spread each array element, keep scalars. Turns
        // an array of arrays (e.g. dictionary column-groups) into one array.
        "flatten" => {
            if !args.is_empty() {
                return Err(HelixError::new("`flatten` takes no arguments", line, col));
            }
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for v in items {
                match v {
                    Value::Array(a) => out.extend(a.to_values().iter().cloned()),
                    other => out.push(other.clone()),
                }
            }
            Ok(Value::array_sniff(out))
        }
        // --- descriptive statistics over one numeric array (missing propagates) ---
        "standard_error" | "coefficient_of_variation" | "iqr" | "spread" | "zscores" => {
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, name, line, col)?;
            if xs.is_empty() {
                return Err(HelixError::new(
                    format!("cannot compute `{name}` of an empty array"),
                    line,
                    col,
                ));
            }
            match name {
                "standard_error" => {
                    Ok(Value::Float(crate::stats::std(&xs) / (xs.len() as f64).sqrt()))
                }
                "coefficient_of_variation" => {
                    let m = crate::stats::mean(&xs);
                    if m == 0.0 {
                        return Err(HelixError::new(
                            "coefficient of variation is undefined: the mean is zero",
                            line,
                            col,
                        ));
                    }
                    Ok(Value::Float(crate::stats::std(&xs) / m))
                }
                "iqr" => Ok(Value::Float(
                    crate::stats::quantile(&xs, 0.75) - crate::stats::quantile(&xs, 0.25),
                )),
                "spread" => {
                    let (mut lo, mut hi) = (xs[0], xs[0]);
                    for &x in &xs {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                    Ok(Value::Float(hi - lo))
                }
                _ => {
                    let (m, sd) = (crate::stats::mean(&xs), crate::stats::std(&xs));
                    if sd == 0.0 {
                        return Err(HelixError::new(
                            "cannot compute z-scores: the values have zero spread",
                            line,
                            col,
                        )
                        .hint("a constant series has no standard deviation to scale by."));
                    }
                    Ok(Value::array(xs.iter().map(|x| Value::Float((x - m) / sd)).collect()))
                }
            }
        }
        // --- sequence helpers over an array of DNA values (missing propagates) ---
        "mean_gc" | "total_length" => {
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let seqs: Vec<&Rc<String>> = items
                .iter()
                .map(|v| match v {
                    Value::Dna(s) => Ok(s),
                    other => Err(HelixError::new(
                        format!(
                            "`{name}` needs an array of DNA sequences, found a {}",
                            other.type_name()
                        ),
                        line,
                        col,
                    )),
                })
                .collect::<Result<_, _>>()?;
            if name == "total_length" {
                Ok(Value::Int(seqs.iter().map(|s| s.len() as i64).sum()))
            } else {
                if seqs.is_empty() {
                    return Err(HelixError::new("cannot compute `mean_gc` of no sequences", line, col));
                }
                let total: f64 =
                    seqs.iter().map(|s| dna_gc(s, name, line, col)).sum::<Result<f64, _>>()?;
                Ok(Value::Float(total / seqs.len() as f64))
            }
        }
        // --- vector math over a numeric array (missing propagates) ---
        "dot" => {
            if args.len() != 1 {
                return Err(HelixError::new("`dot` takes one array argument", line, col));
            }
            let other = match &args[0] {
                Value::Array(a) => a.to_values(),
                Value::Missing => return Ok(Value::Missing),
                o => return Err(HelixError::new(
                    format!("`dot` expects an array, but got a {}", o.type_name()),
                    line,
                    col,
                )),
            };
            if items.iter().chain(other.iter()).any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let (xs, ys) = (numeric_vec(items, "dot", line, col)?, numeric_vec(&other, "dot", line, col)?);
            if xs.len() != ys.len() {
                return Err(HelixError::new(
                    format!("`dot` needs equal-length arrays, got {} and {}", xs.len(), ys.len()),
                    line,
                    col,
                ));
            }
            Ok(Value::Float(xs.iter().zip(&ys).map(|(a, b)| a * b).sum()))
        }
        "norm" => {
            if !args.is_empty() {
                return Err(HelixError::new("`norm` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "norm", line, col)?;
            Ok(Value::Float(xs.iter().map(|x| x * x).sum::<f64>().sqrt()))
        }
        "cumsum" => {
            if !args.is_empty() {
                return Err(HelixError::new("`cumsum` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Preserve int-ness: an all-Int array cumsums to ints.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let mut acc = 0i64;
                let out: Vec<i64> = items
                    .iter()
                    .map(|v| {
                        if let Value::Int(i) = v {
                            acc = acc.wrapping_add(*i);
                        }
                        acc
                    })
                    .collect();
                Ok(Value::int_array(out))
            } else {
                let xs = numeric_vec(items, "cumsum", line, col)?;
                let mut acc = 0.0;
                Ok(Value::float_array(xs.iter().map(|x| { acc += x; acc }).collect()))
            }
        }
        "product" => {
            if !args.is_empty() {
                return Err(HelixError::new("`product` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let p = items.iter().fold(1i64, |acc, v| {
                    if let Value::Int(i) = v { acc.wrapping_mul(*i) } else { acc }
                });
                Ok(Value::Int(p))
            } else {
                let xs = numeric_vec(items, "product", line, col)?;
                Ok(Value::Float(xs.iter().product()))
            }
        }
        // --- ML helpers (missing propagates) ---
        "argsort" => {
            if !args.is_empty() {
                return Err(HelixError::new("`argsort` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let mut idx: Vec<i64> = (0..items.len() as i64).collect();
            // Stable ascending sort of the *indices* by the values they point at -
            // numeric (exact, like `sort`) or all-string; other element types error.
            if items.iter().all(|v| v.as_f64().is_some()) {
                idx.sort_by(|&a, &b| numeric_cmp(&items[a as usize], &items[b as usize]));
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                idx.sort_by(|&a, &b| match (&items[a as usize], &items[b as usize]) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                return Err(HelixError::new(
                    "`argsort` needs an array of all numbers or all strings",
                    line,
                    col,
                ));
            }
            Ok(Value::int_array(idx))
        }
        "clamp" => {
            if args.len() != 2 {
                return Err(HelixError::new("`clamp` takes (lo, hi)", line, col));
            }
            let lo = args[0].as_f64().ok_or_else(|| {
                HelixError::new("`clamp` lo must be a number", line, col)
            })?;
            let hi = args[1].as_f64().ok_or_else(|| {
                HelixError::new("`clamp` hi must be a number", line, col)
            })?;
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Preserve int-ness when all elements (and bounds) are integral.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let (loi, hii) = (lo as i64, hi as i64);
                let out: Vec<i64> = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { (*i).clamp(loi, hii) } else { 0 })
                    .collect();
                Ok(Value::int_array(out))
            } else {
                let xs = numeric_vec(items, "clamp", line, col)?;
                Ok(Value::float_array(xs.iter().map(|x| x.clamp(lo, hi)).collect()))
            }
        }
        "softmax" => {
            if !args.is_empty() {
                return Err(HelixError::new("`softmax` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "softmax", line, col)?;
            if xs.is_empty() {
                return Ok(Value::float_array(Vec::new()));
            }
            // Subtract the max for numerical stability.
            let m = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let exps: Vec<f64> = xs.iter().map(|x| (x - m).exp()).collect();
            let total: f64 = exps.iter().sum();
            Ok(Value::float_array(exps.iter().map(|e| e / total).collect()))
        }
        "bootstrap" => crate::rng::bootstrap(items, args, line, col),
        "contains" => {
            if args.len() != 1 {
                return Err(HelixError::new("`contains` takes one value to look for", line, col));
            }
            Ok(Value::Bool(items.iter().any(|v| values_equal(v, &args[0]))))
        }
        // --- charts + tabular export/write → shared dispatch (rebuild the receiver) ---
        "bar_chart" | "histogram" | "line_chart" | "sparkline" | "scatter" | "svg_bar"
        | "svg_line" | "write_csv" | "write_tsv" | "write_json" | "to_html" | "to_markdown"
        | "to_table" | "write_fasta" | "write_fastq" => {
            export_method(Value::array(items.to_vec()), name, args, line, col)
        }
        // --- reproducible sampling (seeded) ---
        "shuffle" => crate::rng::shuffle(items, args, line, col),
        "sample" => crate::rng::sample(items, args, line, col),
        "choice" => crate::rng::choice(items, args, line, col),
        _ => Err(unknown_method(
            "Array",
            name,
            &crate::registry::methods_of(crate::registry::ARRAY_METHODS),
            line,
            col,
        )),
    }
}

/// Value-count histogram, sorted by count desc then value asc — the shared core
/// of `top`/`frequencies`. String/DNA arrays (k-mer spectra) take a fast ~O(n)
/// hash path; everything else falls back to the value-equality scan (which honors
/// cross-type numeric equality, e.g. `1 == 1.0`), preserving exact semantics.
/// Insertion order is preserved before the sort, matching the old `top`.
fn value_histogram(items: &[Value]) -> Vec<(Value, i64)> {
    let mut counts: Vec<(Value, i64)> = Vec::new();
    if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
        let mut idx: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(items.len());
        for v in items.iter() {
            match idx.get(&v.to_string()) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    idx.insert(v.to_string(), counts.len());
                    counts.push((v.clone(), 1));
                }
            }
        }
    } else {
        for v in items.iter() {
            if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                e.1 += 1;
            } else {
                counts.push((v.clone(), 1));
            }
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string())));
    counts
}

fn empty_guard(xs: &[f64], who: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if xs.is_empty() {
        Err(HelixError::new(
            format!("cannot compute `{}` of an empty array", who),
            line,
            col,
        ))
    } else {
        Ok(())
    }
}

/// Neumaier's improved Kahan compensated summation — bounds the rounding error of
/// a float sum, recovering terms that naive left-to-right summation would lose to
/// catastrophic cancellation. Every float aggregation routes through it.
/// Neumaier compensated summation (low rounding error). Past a threshold it sums
/// FIXED-size chunks in parallel and combines the (compensated) partials in chunk
/// order — same accuracy, and the same result on every machine/core count, because
/// the chunk boundaries depend only on the length, never on the thread pool.
pub(crate) fn neumaier_sum(xs: &[f64]) -> f64 {
    // 256k-element chunks; below 2 chunks it isn't worth a thread hand-off, and the
    // result is then bit-identical to the old sequential path (no value churn for
    // typical small/medium arrays).
    const CHUNK: usize = 1 << 18;
    if xs.len() < CHUNK * 2 {
        return neumaier_seq(xs);
    }
    use rayon::prelude::*;
    let partials: Vec<f64> = xs.par_chunks(CHUNK).map(neumaier_seq).collect();
    neumaier_seq(&partials)
}

fn neumaier_seq(xs: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut c = 0.0; // running compensation for lost low-order bits
    for &x in xs {
        let t = sum + x;
        if sum.abs() >= x.abs() {
            c += (sum - t) + x;
        } else {
            c += (x - t) + sum;
        }
        sum = t;
    }
    sum + c
}

fn population_std(xs: &[f64]) -> f64 {
    let mean = neumaier_sum(xs) / xs.len() as f64;
    let sq: Vec<f64> = xs.iter().map(|x| (x - mean).powi(2)).collect();
    let var = neumaier_sum(&sq) / xs.len() as f64;
    var.sqrt()
}

fn string_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    // Arity check; methods that take arguments call it with their own count.
    let arity = |n: usize| -> Result<(), HelixError> {
        if args.len() != n {
            return Err(HelixError::new(
                format!(
                    "`{}` expects {} argument{}, got {}",
                    name,
                    n,
                    if n == 1 { "" } else { "s" },
                    args.len()
                ),
                line,
                col,
            ));
        }
        Ok(())
    };
    match name {
        "upper" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_uppercase())))
        }
        "lower" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.to_lowercase())))
        }
        "count" | "length" => {
            arity(0)?;
            Ok(Value::Int(s.chars().count() as i64))
        }
        "reverse" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.chars().rev().collect())))
        }
        "trim" => {
            arity(0)?;
            Ok(Value::Str(Rc::new(s.trim().to_string())))
        }
        // Text layout: `repeat` builds separators (`"-".repeat(64)`); `ljust`/`rjust`/
        // `center` pad to a width with spaces — for *computed* widths (the `{x:20}`
        // format spec only takes a literal width), e.g. aligning a column to the
        // longest label. Padding measures Unicode scalar count, like the format spec.
        // `take(n)` / `drop(n)` — first n characters / all but the first n, mirroring the
        // Array methods (slicing `s[a:b]` also works; these are the prefix shorthand).
        // Counted by Unicode scalar, so they're correct on non-ASCII text.
        "take" | "drop" => {
            arity(1)?;
            let n = match &args[0] {
                Value::Int(n) if *n >= 0 => *n as usize,
                Value::Int(_) => {
                    return Err(HelixError::new(format!("`{name}` needs a non-negative count"), line, col))
                }
                other => return Err(type_err(name, "an integer count", other, line, col)),
            };
            let out: String = if name == "take" {
                s.chars().take(n).collect()
            } else {
                s.chars().skip(n).collect()
            };
            Ok(Value::Str(Rc::new(out)))
        }
        "repeat" => {
            arity(1)?;
            let n = match &args[0] {
                Value::Int(n) if *n >= 0 => *n as usize,
                Value::Int(_) => {
                    return Err(HelixError::new("`repeat` needs a non-negative count", line, col))
                }
                other => return Err(type_err("repeat", "an integer count", other, line, col)),
            };
            if s.len().saturating_mul(n) > crate::interp::MAX_STRING_LEN {
                return Err(HelixError::new(
                    format!("`repeat` would exceed {} bytes", crate::interp::MAX_STRING_LEN),
                    line,
                    col,
                )
                .hint("use a smaller count."));
            }
            Ok(Value::Str(Rc::new(s.repeat(n))))
        }
        "ljust" | "rjust" | "center" => {
            arity(1)?;
            const MAX_PAD: usize = 1 << 20;
            let width = match &args[0] {
                Value::Int(w) if *w >= 0 && (*w as usize) <= MAX_PAD => *w as usize,
                Value::Int(w) if *w < 0 => {
                    return Err(HelixError::new(format!("`{name}` needs a non-negative width"), line, col))
                }
                Value::Int(_) => {
                    return Err(HelixError::new(format!("`{name}` width is too large (max {MAX_PAD})"), line, col))
                }
                other => return Err(type_err(name, "an integer width", other, line, col)),
            };
            let len = s.chars().count();
            if len >= width {
                return Ok(Value::Str(s.clone()));
            }
            let fill = width - len;
            let padded = match name {
                "ljust" => format!("{s}{}", " ".repeat(fill)),
                "rjust" => format!("{}{s}", " ".repeat(fill)),
                _ => {
                    let l = fill / 2;
                    format!("{}{s}{}", " ".repeat(l), " ".repeat(fill - l))
                }
            };
            Ok(Value::Str(Rc::new(padded)))
        }
        "split" => {
            arity(1)?;
            let sep = str_arg(args, 0, name, line, col)?;
            if sep.is_empty() {
                return Err(HelixError::new("`split` separator cannot be empty", line, col)
                    .hint("split on a non-empty string, e.g. `s.split(\",\")`."));
            }
            window_count_guard("split", s.matches(sep).count() + 1, line, col)?;
            let parts: Vec<Value> =
                s.split(sep).map(|p| Value::Str(Rc::new(p.to_string()))).collect();
            Ok(Value::array(parts))
        }
        "replace" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replace(from, to))))
        }
        "contains" => {
            arity(1)?;
            Ok(Value::Bool(s.contains(str_arg(args, 0, name, line, col)?)))
        }
        "starts_with" => {
            arity(1)?;
            Ok(Value::Bool(s.starts_with(str_arg(args, 0, name, line, col)?)))
        }
        "ends_with" => {
            arity(1)?;
            Ok(Value::Bool(s.ends_with(str_arg(args, 0, name, line, col)?)))
        }
        "phred" => {
            // Decode a FASTQ Phred+33 quality string to per-base integer quality
            // scores (each character's ASCII value minus 33, the Sanger/Illumina-1.8+
            // encoding). Composes with the array verbs — `read.qual.phred().mean()`
            // is a read's mean quality; `read.qual` is `missing` propagates here too.
            arity(0)?;
            let mut scores: Vec<i64> = Vec::with_capacity(s.len());
            for (i, b) in s.bytes().enumerate() {
                if !(33..=126).contains(&b) {
                    return Err(HelixError::new(
                        format!(
                            "`phred` found a non-quality byte {b} at position {i}; a Phred+33 \
                             quality string uses the printable characters '!' (0) through '~' (93)"
                        ),
                        line,
                        col,
                    ));
                }
                scores.push((b - 33) as i64);
            }
            Ok(Value::int_array(scores))
        }
        "parse_json" => {
            arity(0)?;
            crate::json::parse(s).map_err(|e| HelixError::new(e, line, col))
        }
        // `"3.14".to_float()` / `"42".to_int()`: parse a numeric string. Same impl as the
        // free functions `to_float`/`to_int`, so both spellings agree.
        "to_float" => {
            arity(0)?;
            crate::interp::builtins::parse_str_float(s, line, col)
        }
        "to_int" => {
            arity(0)?;
            crate::interp::builtins::parse_str_int(s, line, col)
        }
        // `text.write_to(path)` / `append_to(path)`: the receiver is the text and the
        // argument is the path (the reverse of the underlying `writers` arg order).
        "write_to" | "append_to" => {
            arity(1)?;
            // Capability gate (ADR 0021): writing text to a path is `FsWrite` authority.
            crate::capability::gate_method(name, args, line, col)?;
            let path = args[0].clone();
            let a = vec![path, Value::Str(s.clone())];
            if name == "write_to" {
                crate::writers::write_text(&a, line, col)
            } else {
                crate::writers::append_text(&a, line, col)
            }
        }
        _ => Err(unknown_method(
            "String",
            name,
            &crate::registry::methods_of(crate::registry::STRING_METHODS),
            line,
            col,
        )),
    }
}

/// Pull argument `i` as a `&str`, with a clean type error otherwise.
fn str_arg<'a>(
    args: &'a [Value],
    i: usize,
    who: &str,
    line: usize,
    col: usize,
) -> Result<&'a str, HelixError> {
    match &args[i] {
        Value::Str(a) => Ok(a.as_str()),
        other => Err(type_err(who, "a string", other, line, col)),
    }
}

fn dna_method(
    s: &Rc<String>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "length" | "count" => {
            if !args.is_empty() {
                return Err(HelixError::new(format!("`{}` takes no arguments", name), line, col));
            }
            Ok(Value::Int(s.len() as i64))
        }
        "gc_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_content` takes no arguments", line, col));
            }
            if s.is_empty() {
                return Err(HelixError::new(
                    "cannot compute `gc_content` of an empty sequence",
                    line,
                    col,
                ));
            }
            // GC fraction over *called* bases: `N` (unknown) is excluded from the
            // denominator, so `gc_content("GCN") == 1.0`, not 2/3. `Dna` is ASCII, so count
            // raw bytes (AVX2 when available, else auto-vectorized scalar) — `called =
            // len - Ns`.
            let bytes = s.as_bytes();
            let (gc, ns) = crate::simd::gc_counts(bytes);
            let called = bytes.len() as i64 - ns;
            Ok(Value::Float(if called == 0 { 0.0 } else { gc as f64 / called as f64 }))
        }
        "complement" => {
            if !args.is_empty() {
                return Err(HelixError::new("`complement` takes no arguments", line, col));
            }
            Ok(Value::Dna(Rc::new(complement(s))))
        }
        "reverse_complement" => {
            if !args.is_empty() {
                return Err(HelixError::new(
                    "`reverse_complement` takes no arguments",
                    line,
                    col,
                ));
            }
            // One pass, one allocation: write the complement of byte `i` straight into the
            // reversed output slot (`complement(s).chars().rev()` was two passes + two
            // allocations). Byte-reverse equals char-reverse for ASCII (always, for DNA).
            let rc = if s.is_ascii() {
                let lut = complement_lut();
                let bytes = s.as_bytes();
                let n = bytes.len();
                let mut out = vec![0u8; n];
                for (i, &b) in bytes.iter().enumerate() {
                    out[n - 1 - i] = lut[b as usize];
                }
                // SAFETY: ASCII in, LUT maps ASCII→ASCII, so every output byte is valid UTF-8.
                unsafe { String::from_utf8_unchecked(out) }
            } else {
                complement(s).chars().rev().collect()
            };
            Ok(Value::Dna(Rc::new(rc)))
        }
        "find" => {
            arity("find", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find` needs a string or DNA pattern, but got a {}", v.type_name()),
                        line,
                        col,
                    ))
                }
            };
            // ACGT is ASCII, so the byte offset is the base offset.
            match s.find(&needle) {
                Some(idx) => Ok(Value::Int(idx as i64)),
                None => Ok(Value::Missing),
            }
        }
        "find_all" => {
            arity("find_all", args, 1, line, col)?;
            let needle = match &args[0] {
                Value::Str(p) => (**p).clone(),
                Value::Dna(p) => (**p).clone(),
                v => {
                    return Err(HelixError::new(
                        format!("`find_all` needs a string or DNA pattern, but got a {}", v.type_name()),
                        line,
                        col,
                    ))
                }
            };
            if needle.is_empty() {
                return Err(HelixError::new("`find_all` needs a non-empty pattern", line, col)
                    .hint("pass the motif you're scanning for, e.g. `seq.find_all(\"GAATTC\")`."));
            }
            // Every 0-based start position, overlapping allowed (advance by 1 past each
            // hit) — the motif-scan / restriction-site convention. ACGT is ASCII so the
            // byte offset is the base offset, and `str::find` is memchr/Two-Way backed,
            // so this is one native O(n) pass instead of materializing n windows.
            let hay = s.as_str();
            let mut positions = Vec::new();
            let mut start = 0usize;
            while let Some(off) = hay[start..].find(needle.as_str()) {
                let idx = start + off;
                positions.push(idx as i64);
                start = idx + 1;
            }
            Ok(Value::int_array(positions))
        }
        "gc_skew" => {
            if !args.is_empty() {
                return Err(HelixError::new("`gc_skew` takes no arguments", line, col));
            }
            // The cumulative GC-skew walk: +1 per G, -1 per C, unchanged on A/T/N. The
            // running total at each base — the classic replication-origin signal, whose
            // minimum marks the ori. One native pass replaces a per-base interpreter loop;
            // exact integers (no float drift). An empty sequence yields `[]`.
            let mut acc: i64 = 0;
            let walk: Vec<i64> = s
                .bytes()
                .map(|b| {
                    match b {
                        b'G' => acc += 1,
                        b'C' => acc -= 1,
                        _ => {}
                    }
                    acc
                })
                .collect();
            Ok(Value::int_array(walk))
        }
        "longest_homopolymer" => {
            if !args.is_empty() {
                return Err(HelixError::new("`longest_homopolymer` takes no arguments", line, col));
            }
            // Length of the longest run of a single identical base — a common QC signal
            // (long homopolymers are a sequencer error mode). One byte pass, no allocation;
            // an empty sequence is `0`. `prev = 0` (NUL) never equals an ASCII base, so the
            // first base correctly starts a run of 1.
            let mut best = 0i64;
            let mut run = 0i64;
            let mut prev = 0u8;
            for &b in s.as_bytes() {
                if b == prev {
                    run += 1;
                } else {
                    run = 1;
                    prev = b;
                }
                if run > best {
                    best = run;
                }
            }
            Ok(Value::Int(best))
        }
        "kmers" => {
            // The countable k-mer *spectrum*: only windows of unambiguous ACGT —
            // any window containing `N`/IUPAC is skipped (the Jellyfish/KMC/KmerGo
            // convention), so every emitted k-mer round-trips through `dna()` and is
            // canonicalizable. A sequence shorter than `k` (or empty) yields `[]`.
            let k = kmer_k("kmers", args, line, col)?;
            // DNA is validated ASCII, so windows are byte slices (no `Vec<char>` build,
            // no per-char decode); a window is unambiguous iff every byte is `ACGT`.
            let bytes = s.as_bytes();
            let mut out = Vec::new();
            if k <= bytes.len() {
                window_count_guard("kmers", bytes.len() - k + 1, line, col)?;
                for w in bytes.windows(k) {
                    if w.iter().all(|&b| matches!(b, b'A' | b'C' | b'G' | b'T')) {
                        // SAFETY: a window of an ASCII DNA string is valid UTF-8.
                        out.push(Value::Str(Rc::new(unsafe { String::from_utf8_unchecked(w.to_vec()) })));
                    }
                }
            }
            Ok(Value::array(out))
        }
        "windows" => {
            // Every length-`k` substring, faithfully (ambiguity included) — the
            // sequence is reconstructable from its windows. Shorter than `k` → `[]`.
            let k = kmer_k("windows", args, line, col)?;
            // DNA is validated ASCII → byte-slice windows (no `Vec<char>`, no decode).
            let bytes = s.as_bytes();
            let mut out = Vec::new();
            if k <= bytes.len() {
                let count = bytes.len() - k + 1;
                window_count_guard("windows", count, line, col)?;
                out.reserve(count);
                for w in bytes.windows(k) {
                    // SAFETY: a window of an ASCII DNA string is valid UTF-8.
                    out.push(Value::Str(Rc::new(unsafe { String::from_utf8_unchecked(w.to_vec()) })));
                }
            }
            Ok(Value::array(out))
        }
        "codons" => {
            if !args.is_empty() {
                return Err(HelixError::new("`codons` takes no arguments", line, col));
            }
            // Split into non-overlapping reading-frame-0 triplets, dropping a trailing
            // partial codon (length not a multiple of 3) — the standard codon iteration
            // for a coding sequence, feeding a `codon -> amino acid` lookup. A `Dna` is
            // ASCII, so step the bytes in chunks of 3 (no per-base decode) and emit one
            // string per codon. A sequence shorter than 3 yields `[]`.
            let bytes = s.as_bytes();
            let count = bytes.len() / 3;
            window_count_guard("codons", count, line, col)?;
            let mut out = Vec::with_capacity(count);
            for chunk in bytes.chunks_exact(3) {
                out.push(Value::Str(Rc::new(String::from_utf8_lossy(chunk).into_owned())));
            }
            Ok(Value::array(out))
        }
        "kmer_counts" => {
            // Native 2-bit-packed k-mer spectrum (k ≤ 32): each ACGT window packs
            // into a u64 — no per-window string allocation — counted in a hash map;
            // only the *distinct* k-mers are decoded to strings at the end. Windows
            // spanning N/IUPAC are skipped (same spectrum as `kmers`). Returns
            // (kmer, count) tuples, count desc then k-mer asc. The fast path for
            // `kmers(k).frequencies()`.
            let k = kmer_k("kmer_counts", args, line, col)?;
            if k > 32 {
                return Err(HelixError::new(
                    format!("`kmer_counts` supports k up to 32 (2-bit packed), got {}", k),
                    line,
                    col,
                )
                .hint("for larger k use `kmers(k).frequencies()`."));
            }
            Ok(Value::array(packed_kmer_counts(s, k, false)))
        }
        "canonical_kmer_counts" => {
            // Strand-agnostic k-mer spectrum: a k-mer and its reverse complement are
            // counted together under their *canonical* form (the lexicographically
            // smaller of the two), so coverage from either strand collapses to one
            // entry — the Jellyfish/KMC `--canonical` convention. Same 2-bit-packed
            // counting as `kmer_counts`; the reverse complement is computed directly
            // on the packed code (complement = `bits ^ 3`, then the bases reversed).
            let k = kmer_k("canonical_kmer_counts", args, line, col)?;
            if k > 32 {
                return Err(HelixError::new(
                    format!("`canonical_kmer_counts` supports k up to 32 (2-bit packed), got {}", k),
                    line,
                    col,
                )
                .hint("for larger k, canonicalize `kmers(k)` yourself before `frequencies()`."));
            }
            Ok(Value::array(packed_kmer_counts(s, k, true)))
        }
        "align" => {
            // `seq.align(target[, mode])` — pairwise alignment (ADR 0015). The result
            // is a plain record so it composes with field access and prints normally.
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`align` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("call `seq.align(target)` or `seq.align(target, \"local\")`."));
            }
            let target = match &args[0] {
                Value::Dna(t) => t,
                other => return Err(type_err("align", "a DNA sequence", other, line, col)),
            };
            let mode = match args.get(1) {
                None => crate::align::Mode::Global,
                Some(Value::Str(m)) => match m.as_str() {
                    "global" => crate::align::Mode::Global,
                    "local" => crate::align::Mode::Local,
                    "semiglobal" => crate::align::Mode::Semiglobal,
                    other => {
                        return Err(HelixError::new(
                            format!("unknown alignment mode `{other}`", ),
                            line,
                            col,
                        )
                        .hint("the modes are \"global\" (default), \"local\", and \"semiglobal\"."))
                    }
                },
                Some(other) => return Err(type_err("align", "a mode string", other, line, col)),
            };
            // Cap the dynamic-programming matrix: it is O(n*m) in both time and memory
            // (six matrices over the (n+1)x(m+1) grid), so a pair of very long sequences
            // would exhaust memory. Reads-vs-genes stay far under this; whole-genome
            // alignment is out of scope (ADR 0015).
            // 50M cells: at i64 scores the six DP matrices are ~27 bytes/cell, so this
            // bounds the table near ~1.3 GB (halved from the old i32 cap to match the
            // wider, overflow-proof score type).
            const MAX_ALIGN_CELLS: usize = 50_000_000;
            let cells = s.len().saturating_mul(target.len());
            if cells > MAX_ALIGN_CELLS {
                return Err(HelixError::new(
                    format!(
                        "`align` would build a {}x{} matrix, too large (keep the product under {})",
                        s.len(),
                        target.len(),
                        MAX_ALIGN_CELLS
                    ),
                    line,
                    col,
                )
                .hint("align shorter sequences, or a region of each."));
            }
            let a = crate::align::align(
                s.as_bytes(),
                target.as_bytes(),
                mode,
                crate::align::Scoring::nucleotide(),
            );
            use crate::symbol::Symbol;
            Ok(Value::Record(Rc::new(vec![
                (Symbol::intern("score"), Value::Int(a.score)),
                (Symbol::intern("cigar"), Value::Str(Rc::new(a.cigar))),
                (Symbol::intern("query"), Value::Str(Rc::new(a.x_aligned))),
                (Symbol::intern("target"), Value::Str(Rc::new(a.y_aligned))),
                (Symbol::intern("start"), Value::Int(a.y_start as i64)),
                (Symbol::intern("end"), Value::Int(a.y_end as i64)),
            ])))
        }
        "at_content" => {
            if !args.is_empty() {
                return Err(HelixError::new("`at_content` takes no arguments", line, col));
            }
            // AT fraction = 1 − GC fraction (over called bases; `N` excluded).
            Ok(Value::Float(1.0 - dna_gc(s, "at_content", line, col)?))
        }
        // Per-base tally in ONE pass over the sequence (no per-base string allocation):
        // `{A, C, G, T, N}` where `N` collects every non-ACGT base. Access via `.A` etc.
        "base_counts" => {
            if !args.is_empty() {
                return Err(HelixError::new("`base_counts` takes no arguments", line, col));
            }
            // A `Dna` is ASCII (validated + upper-cased at construction), so count raw
            // bytes — no UTF-8 decode. `simd::base_counts` uses AVX2 (32 bases/instr) when
            // available, else a branchless auto-vectorized scalar count; both are exact, so
            // `N` (every non-ACGT base) is the remainder, matching the old `_ => n` arm.
            let bytes = s.as_bytes();
            let (a, c, g, t) = crate::simd::base_counts(bytes);
            let n = bytes.len() as i64 - a - c - g - t;
            use crate::symbol::Symbol;
            Ok(Value::Record(Rc::new(vec![
                (Symbol::intern("A"), Value::Int(a)),
                (Symbol::intern("C"), Value::Int(c)),
                (Symbol::intern("G"), Value::Int(g)),
                (Symbol::intern("T"), Value::Int(t)),
                (Symbol::intern("N"), Value::Int(n)),
            ])))
        }
        // Hamming distance: differing positions between two equal-length sequences, in one
        // pass (no per-base slices). The other sequence may be a `Dna` or a `String`.
        "hamming" => {
            arity("hamming", args, 1, line, col)?;
            let other: &str = match &args[0] {
                Value::Dna(o) => o,
                Value::Str(o) => o,
                v => {
                    return Err(HelixError::new(
                        format!("`hamming` needs a DNA or string sequence, but got a {}", v.type_name()),
                        line,
                        col,
                    ))
                }
            };
            // Fast path: both ASCII (always, for a `Dna` receiver vs an ASCII sequence) →
            // compare bytes (the comparison auto-vectorizes, no per-char decode). Falls
            // back to the exact char-based count for any non-ASCII `other`.
            if s.is_ascii() && other.is_ascii() {
                let (sb, ob) = (s.as_bytes(), other.as_bytes());
                if sb.len() != ob.len() {
                    return Err(HelixError::new(
                        format!("`hamming` needs equal-length sequences, got {} and {}", sb.len(), ob.len()),
                        line,
                        col,
                    )
                    .hint("align or trim the sequences to the same length first."));
                }
                let dist = sb.iter().zip(ob).filter(|(x, y)| x != y).count();
                return Ok(Value::Int(dist as i64));
            }
            let (ls, lo) = (s.chars().count(), other.chars().count());
            if ls != lo {
                return Err(HelixError::new(
                    format!("`hamming` needs equal-length sequences, got {ls} and {lo}"),
                    line,
                    col,
                )
                .hint("align or trim the sequences to the same length first."));
            }
            let dist = s.chars().zip(other.chars()).filter(|(x, y)| x != y).count();
            Ok(Value::Int(dist as i64))
        }
        _ => Err(unknown_method(
            "Dna",
            name,
            &crate::registry::methods_of(crate::registry::DNA_METHODS),
            line,
            col,
        )),
    }
}

/// 2-bit-packed k-mer counts (k ≤ 32), as `(kmer, count)` tuples sorted by count
/// desc then k-mer asc. Each ACGT window rolls into a `u64` (A=0 C=1 G=2 T=3) with
/// no allocation; a non-ACGT base breaks the window (the `kmers` spectrum). A string
/// is built only per *distinct* k-mer (decoded at the end), and u64 keys hash far
/// faster than strings — the native fast path for `kmers(k).frequencies()`. Same
/// fixed width means u64 order == lexicographic k-mer order, so sorting by the packed
/// code matches a string sort. When `canonical`, each window is counted under
/// `min(code, reverse_complement(code))`, collapsing the two strands into one entry.
fn packed_kmer_counts(s: &str, k: usize, canonical: bool) -> Vec<Value> {
    let mask: u64 = if k >= 32 { u64::MAX } else { (1u64 << (2 * k)) - 1 };
    let mut code: u64 = 0;
    let mut valid: usize = 0;
    // FxHashMap (fast non-cryptographic hash) — u64 keys hash in a couple of ops,
    // the point of packing vs hashing 4.6M strings.
    let mut counts: rustc_hash::FxHashMap<u64, u64> = rustc_hash::FxHashMap::default();
    for byte in s.bytes() {
        let bits = match byte {
            b'A' => 0u64,
            b'C' => 1,
            b'G' => 2,
            b'T' => 3,
            _ => {
                valid = 0; // ambiguous base — break the window
                continue;
            }
        };
        code = ((code << 2) | bits) & mask;
        valid += 1;
        if valid >= k {
            let key = if canonical { code.min(revcomp_code(code, k)) } else { code };
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    let mut pairs: Vec<(u64, u64)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs
        .into_iter()
        .map(|(c, n)| {
            let mut km = String::with_capacity(k);
            for i in 0..k {
                let b = (c >> (2 * (k - 1 - i))) & 3;
                km.push([b'A', b'C', b'G', b'T'][b as usize] as char);
            }
            Value::Tuple(Rc::new(vec![Value::Str(Rc::new(km)), Value::Int(n as i64)]))
        })
        .collect()
}

/// The reverse complement of a 2-bit-packed `k`-mer code (A=0 C=1 G=2 T=3). Each
/// base is complemented by `bits ^ 3` (A↔T, C↔G) and the bases are emitted in
/// reverse order, so the result is itself a valid `k`-base packed code.
fn revcomp_code(mut code: u64, k: usize) -> u64 {
    let mut rc: u64 = 0;
    for _ in 0..k {
        let base = code & 3;
        rc = (rc << 2) | (base ^ 3);
        code >>= 2;
    }
    rc
}

/// Parse the single positive-length argument shared by `kmers`/`windows`.
/// The shared upper bound on a user-controllable output element count (matches the
/// `range` cap). Past this, an op errors cleanly rather than OOM-aborting.
pub(crate) const MAX_ELEMENTS: usize = 100_000_000;

/// Guard the number of substrings a `kmers`/`windows`/`split` call would emit, so a
/// huge input errors cleanly instead of allocating tens of GB of `Value::Str`.
fn window_count_guard(name: &str, count: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if count > MAX_ELEMENTS {
        return Err(HelixError::new(
            format!("`{name}` would produce {count} substrings, too many to hold in memory"),
            line,
            col,
        )
        .hint("use a longer k, a shorter input, or `kmer_counts(k)` for the spectrum."));
    }
    Ok(())
}

fn kmer_k(name: &str, args: &[Value], line: usize, col: usize) -> Result<usize, HelixError> {
    arity(name, args, 1, line, col)?;
    let k = as_int(&args[0], name, line, col)?;
    if k <= 0 {
        return Err(HelixError::new(
            format!("`{}` needs a positive length, got {}", name, k),
            line,
            col,
        ));
    }
    Ok(k as usize)
}

/// A valid (uppercase) IUPAC nucleotide code: the 4 bases, the 10 two/three-fold
/// ambiguity codes, and `N` (any base). This is the alphabet `dna()` accepts and
/// `read_fasta`/`read_fastq` already produce, so the two paths agree.
pub(crate) fn is_iupac_dna(c: char) -> bool {
    matches!(
        c,
        'A' | 'C' | 'G' | 'T' | 'R' | 'Y' | 'S' | 'W' | 'K' | 'M' | 'B' | 'D' | 'H' | 'V' | 'N'
    )
}

/// IUPAC complement of one (uppercase) base. Ambiguity codes complement to the
/// code for the complementary base set (`R`=A/G → `Y`=C/T, etc.); `S`/`W`/`N` are
/// self-complementary. Unknown chars pass through unchanged (defensive).
fn iupac_complement(c: char) -> char {
    match c {
        'A' => 'T',
        'T' => 'A',
        'C' => 'G',
        'G' => 'C',
        'R' => 'Y',
        'Y' => 'R',
        'K' => 'M',
        'M' => 'K',
        'B' => 'V',
        'V' => 'B',
        'D' => 'H',
        'H' => 'D',
        'S' => 'S',
        'W' => 'W',
        'N' => 'N',
        other => other,
    }
}

/// A 256-entry byte lookup table for the IUPAC complement: each mapped uppercase code
/// (A↔T, C↔G, R↔Y, K↔M, B↔V, D↔H; S/W/N self-complementary) to its complement, identity
/// for every other byte. DNA is validated ASCII, so a per-byte map is exactly equivalent
/// to the per-char [`iupac_complement`] but branchless and vectorizable. Built once.
fn complement_lut() -> &'static [u8; 256] {
    static LUT: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 256];
        for (i, e) in t.iter_mut().enumerate() {
            *e = i as u8; // identity for every unmapped byte (matches `other => other`)
        }
        for (k, v) in [
            (b'A', b'T'), (b'T', b'A'), (b'C', b'G'), (b'G', b'C'), (b'R', b'Y'), (b'Y', b'R'),
            (b'K', b'M'), (b'M', b'K'), (b'B', b'V'), (b'V', b'B'), (b'D', b'H'), (b'H', b'D'),
            (b'S', b'S'), (b'W', b'W'), (b'N', b'N'),
        ] {
            t[k as usize] = v;
        }
        t
    })
}

fn complement(s: &str) -> String {
    // Fast path for ASCII (always, for a validated DNA string): a branchless byte LUT in
    // one pass. The fallback keeps exact behaviour for any non-ASCII input.
    if s.is_ascii() {
        let lut = complement_lut();
        let bytes: Vec<u8> = s.bytes().map(|b| lut[b as usize]).collect();
        // SAFETY: input is ASCII and the LUT maps each ASCII byte to another ASCII byte,
        // so every output byte is valid single-byte UTF-8.
        unsafe { String::from_utf8_unchecked(bytes) }
    } else {
        s.chars().map(iupac_complement).collect()
    }
}

fn unknown_method(
    type_name: &str,
    name: &str,
    candidates: &[&str],
    line: usize,
    col: usize,
) -> HelixError {
    let mut err = HelixError::new(
        format!("a {} has no method `{}`", type_name, name),
        line,
        col,
    );
    if let Some(s) = suggest(name, candidates) {
        err = err.hint(format!("did you mean `{}`?", s));
    } else {
        err = err.hint(format!(
            "available {} methods: {}",
            type_name,
            candidates.join(", ")
        ));
    }
    err
}
