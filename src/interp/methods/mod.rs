//! Value-method dispatch (`call_method`) and the per-type method implementations
//! for arrays, strings, and DNA, plus the shared numeric helpers (Neumaier
//! compensated summation, population standard deviation). These are free
//! functions shared by both the tree-walker and the bytecode VM — the parent
//! module re-exports them, so `crate::interp::call_method` still resolves.

use super::*;

use crate::error::HelixError;
use crate::value::Value;



mod array;
mod string;
mod dna;
mod net;
mod dictrec;
mod headers;

#[allow(unused_imports)]
pub(crate) use array::*;
#[allow(unused_imports)]
pub(crate) use string::*;
#[allow(unused_imports)]
pub(crate) use dna::*;
#[allow(unused_imports)]
pub(crate) use net::*;
#[allow(unused_imports)]
pub(crate) use dictrec::*;
#[allow(unused_imports)]
pub(crate) use headers::*;

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

/// `is_missing` on a DataFrame/GroupBy receiver. Those receivers route to verb
/// dispatch and never reach the universal handler in `call_method`, so both the VM and
/// the tree-walker intercept `is_missing` themselves — a frame/group is never `missing`,
/// so the answer is `false`. One definition keeps the two engines byte-identical here
/// (same value, same arity-error wording).
pub(crate) fn df_is_missing(args_empty: bool, line: usize, col: usize) -> Result<Value, HelixError> {
    if args_empty {
        Ok(Value::Bool(false))
    } else {
        Err(HelixError::new("`is_missing` takes no arguments", line, col))
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
    // `$arg_extreme(want_max)` — the packed kernel behind `argmin`/`argmax`. Unwritable from
    // source (`$` does not lex), absent from `registry::ARRAY_METHODS` so it never appears in
    // `helix doc`, `helix describe` or an unknown-method hint, and answering `missing` means
    // DECLINE — the desugar's `??` then runs the tuple reduce that ran before.
    //
    // ITS POSITION IS LOAD-BEARING. Sitting AFTER the missing-propagation return above is
    // what makes `missing.argmax()` decline for free and keep leaking "a value of type
    // Missing cannot be indexed" from the reduce seed. Hand-writing a Missing case above the
    // rule would RE-DERIVE that behaviour instead of preserving it, which is precisely how a
    // second spelling drifts from the first.
    //
    // Living in `call_method` means both `vm.rs` and `interp.rs` reach it, so one
    // implementation serves all three engines by construction. (The JIT never sees method
    // calls at all.)
    if name == "$arg_extreme" {
        let want_max = matches!(args.first(), Some(Value::Bool(true)));
        return Ok(match recv {
            Value::Array(items) => {
                packed_arg_extreme(items, want_max).map_or(Value::Missing, Value::Int)
            }
            _ => Value::Missing,
        });
    }
    // If a method argument is tracked (autodiff) but the receiver is a plain number
    // or tensor, lift the receiver into the graph too — so `X.matmul(w)` differentiates
    // through `w` even though `X` is a constant. Gated on the TAPE'S OWN method
    // names: an un-gated lift hijacked every method a tracked argument touched
    // (`tensor(..).solve(variable(..))` reported "no differentiable method
    // `solve`" instead of solve's own error — the dx-plan's name-blind-lift item).
    if !matches!(recv, Value::Node(_))
        && crate::autodiff::is_tape_method(name)
        && args.iter().any(|a| matches!(a, Value::Node(_)))
        && let Some(n) = crate::autodiff::lift(recv)
    {
        return crate::autodiff::method(&n, name, &args, line, col);
    }
    match recv {
        Value::Array(items) => {
            // `enumerate()` wraps the receiver LAZILY: element `i` is `(i, items[i])`,
            // produced on demand by `ArrayData::Enumerate` (sharing the receiver's `Rc`),
            // so `xs.enumerate().map(...)` never materializes the O(N)-tuple `Vec`. Handled
            // here (not in `array_numeric_fast`, which only borrows `&ArrayData`) because we
            // need the `Rc` to wrap without copying.
            if name == "enumerate" && args.is_empty() {
                return Ok(Value::Array(std::rc::Rc::new(crate::value::ArrayData::Enumerate {
                    inner: items.clone(),
                })));
            }
            // `zip(ys)` is `enumerate`'s symmetric twin and was the one that never got the
            // treatment: `a.zip(a).length()` on a 5M range cost **631 MB** against the
            // enumerate analogue's 15 MB, because the general path below materialized a
            // `Vec` of `Rc<Vec<Value>>` tuples before anything downstream could decline it.
            //
            // IT HAS TO BE HERE, not in `array_numeric_fast`, and not one line further down.
            // `array_method(&items.to_values(), …)` further down IS the 631 MB; every
            // "cheap interim fast path" placed after it measures identically (zip.first(),
            // zip.last(), zip.count() and zip.length() were all ~646 MB, to within 0.1%).
            // And like `enumerate`, it needs the receiver's `Rc` to share rather than copy,
            // which `array_numeric_fast`'s `&ArrayData` cannot give.
            //
            // ERROR ORDER IS PRESERVED DELIBERATELY: `arity` first (so `zip()` and
            // `zip(a, b)` keep their arity error), then the argument-type check with the
            // identical message and hint the eager arm produces. Firing only when
            // `args.len() == 1` would swallow the arity error.
            if name == "zip" {
                arity("zip", &args, 1, line, col)?;
                let b = match &args[0] {
                    Value::Array(b) => b.clone(),
                    v => {
                        return Err(HelixError::new(
                            format!(
                                "`zip` needs an array, but got {}",
                                crate::value::with_article(v.type_name())
                            ),
                            line,
                            col,
                        )
                        .hint("e.g. `xs.zip(ys)` pairs elements positionally."))
                    }
                };
                // `min` ONCE, here — see the variant's invariant 1. Truncation to the
                // shorter side is thereby a stored fact rather than a re-derived one.
                let len = items.len().min(b.len());
                return Ok(Value::Array(std::rc::Rc::new(crate::value::ArrayData::Zip {
                    a: items.clone(),
                    b,
                    len,
                })));
            }
            // `concat` over PACKED numeric arrays, before the general path boxes anything.
            // The general path costs three passes per call: `to_values()` boxes the
            // receiver into a `Vec<Value>`, `items.to_vec()` clones that, and
            // `array_sniff` unboxes the result back to packed — 16 bytes per element
            // moved twice to append to a buffer of 8-byte elements. This is one
            // allocation and a memcpy per input.
            //
            // Same result as the general path by construction: `array_sniff` on an
            // all-`Int` (all-`Float`) `Vec<Value>` produces exactly `Ints` (`Floats`) with
            // the same elements in the same order. A `Range` receiver or argument is
            // included because `to_ints` materializes it to the same integers.
            //
            // NOT a complexity fix. `xs.concat([x])` in a loop is still O(n^2) — the
            // receiver is copied every call — because the receiver is behind a shared
            // `Rc` (the caller's binding plus the stack value), so it cannot be extended
            // in place. Making THAT O(1) needs last-use liveness in the compiler so the
            // final read of a binding moves instead of cloning; see docs/ROADMAP.md.
            if name == "concat" && !args.is_empty() {
                if let Some(head) = items.to_ints() {
                    let mut tails = Vec::with_capacity(args.len());
                    for a in &args {
                        match a {
                            Value::Array(arr) => match arr.to_ints() {
                                Some(t) => tails.push(t),
                                None => {
                                    tails.clear();
                                    break;
                                }
                            },
                            // A non-array argument is an ERROR, and the general path owns
                            // its exact wording — fall through rather than duplicate it.
                            _ => {
                                tails.clear();
                                break;
                            }
                        }
                    }
                    if tails.len() == args.len() {
                        let total = head.len() + tails.iter().map(|t| t.len()).sum::<usize>();
                        let mut out: Vec<i64> = Vec::with_capacity(total);
                        out.extend_from_slice(&head);
                        for t in &tails {
                            out.extend_from_slice(t);
                        }
                        return Ok(Value::Array(std::rc::Rc::new(
                            crate::value::ArrayData::Ints(out),
                        )));
                    }
                }
                if let crate::value::ArrayData::Floats(head) = &**items {
                    let mut tails: Vec<&Vec<f64>> = Vec::with_capacity(args.len());
                    for a in &args {
                        match a {
                            Value::Array(arr) => match &**arr {
                                crate::value::ArrayData::Floats(t) => tails.push(t),
                                _ => {
                                    tails.clear();
                                    break;
                                }
                            },
                            _ => {
                                tails.clear();
                                break;
                            }
                        }
                    }
                    if tails.len() == args.len() {
                        let total = head.len() + tails.iter().map(|t| t.len()).sum::<usize>();
                        let mut out: Vec<f64> = Vec::with_capacity(total);
                        out.extend_from_slice(head);
                        for t in &tails {
                            out.extend_from_slice(t);
                        }
                        return Ok(Value::Array(std::rc::Rc::new(
                            crate::value::ArrayData::Floats(out),
                        )));
                    }
                }
            }
            // `unique` on a PACKED array must not round-trip through `to_values()`: the
            // general dispatch below boxes every element first, so 80M packed ints became
            // 1.9 GB of `Value`s — and an allocator ABORT under a memory cap — before
            // `unique` ran a single comparison. The zip/enumerate lesson, one method over.
            //
            // On the packed buffer the key IS the scalar: an `Ints`/`Floats` buffer can
            // hold no `missing` and no second type, so a plain `HashSet` reproduces
            // `values_equal`'s classes exactly — with `FloatKey`'s two wrinkles kept:
            // ±0.0 hash together (first seen stays the representative) and NaN belongs
            // to no class at all, so every NaN survives. A `Range` is distinct by
            // construction: its `unique` is itself, same `Rc`, no work. Growth is
            // FALLIBLE throughout (ADR 0024): 90M distinct ints are a legitimate ask on
            // a big machine and a clean error on a small one, never a dead process.
            // Only the bare call takes the fast path — `unique(x)` falls through so the
            // general arm's arity error is preserved word for word.
            if name == "unique" && args.is_empty() {
                use crate::value::{ArrayData, MaterializeLimit};
                fn grow<T>(v: &mut Vec<T>, line: usize, col: usize) -> Result<(), HelixError> {
                    if v.len() == v.capacity() {
                        let more = v.capacity().max(16);
                        v.try_reserve(more).map_err(|_| {
                            crate::vm::materialize_refused(
                                MaterializeLimit::Alloc(
                                    (v.capacity() + more) * std::mem::size_of::<T>(),
                                ),
                                line,
                                col,
                            )
                        })?;
                    }
                    Ok(())
                }
                fn grow_set<T: std::hash::Hash + Eq>(
                    s: &mut std::collections::HashSet<T>,
                    line: usize,
                    col: usize,
                ) -> Result<(), HelixError> {
                    if s.len() == s.capacity() {
                        let more = s.len().max(16);
                        s.try_reserve(more).map_err(|_| {
                            crate::vm::materialize_refused(
                                MaterializeLimit::Alloc(
                                    (s.len() + more) * std::mem::size_of::<T>() * 2,
                                ),
                                line,
                                col,
                            )
                        })?;
                    }
                    Ok(())
                }
                match items.as_ref() {
                    ArrayData::Range { .. } => return Ok(Value::Array(items.clone())),
                    ArrayData::Ints(xs) => {
                        let mut seen: std::collections::HashSet<i64> =
                            std::collections::HashSet::new();
                        let mut out: Vec<i64> = Vec::new();
                        for &x in xs {
                            grow_set(&mut seen, line, col)?;
                            if seen.insert(x) {
                                grow(&mut out, line, col)?;
                                out.push(x);
                            }
                        }
                        return Ok(Value::Array(std::rc::Rc::new(ArrayData::Ints(out))));
                    }
                    ArrayData::Floats(xs) => {
                        let mut seen: std::collections::HashSet<u64> =
                            std::collections::HashSet::new();
                        let mut out: Vec<f64> = Vec::new();
                        for &x in xs {
                            if x.is_nan() {
                                grow(&mut out, line, col)?;
                                out.push(x);
                                continue;
                            }
                            let bits = if x == 0.0 { 0.0f64 } else { x }.to_bits();
                            grow_set(&mut seen, line, col)?;
                            if seen.insert(bits) {
                                grow(&mut out, line, col)?;
                                out.push(x);
                            }
                        }
                        return Ok(Value::Array(std::rc::Rc::new(ArrayData::Floats(out))));
                    }
                    // `Values` (and the lazy tuple views) keep the general path below.
                    _ => {}
                }
            }
            match array_numeric_fast(items, name, &args, line, col)? {
                // A typed array's numeric reduction reads the packed buffer directly.
                Some(v) => Ok(v),
                // Everything else materializes to `Value`s and runs the general path.
                None => array_method(&items.to_values(), name, &args, line, col),
            }
        }
        Value::Str(s) => string_method(s, name, &args, line, col),
        Value::Dna(s) => dna_method(s, name, &args, line, col),
        Value::Node(n) => crate::autodiff::method(n, name, &args, line, col),
        Value::Tensor(t) => crate::tensor::method(t, name, &args, line, col),
        Value::PyObject(h) => crate::python::method(h, name, &args, line, col),
        Value::Dict(map) => dict_method(map, name, &args, line, col),
        Value::Net(h) => net_method(h, name, &args, line, col),
        Value::Headers(hs) => headers_method(hs, name, &args, line, col),
        Value::Record(fields) => record_method(fields, name, &args, line, col),
        other => Err(HelixError::new(
            format!("{} has no method `{}`", crate::value::with_article(other.type_name()), name),
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
                        "`{}` needs an array of numbers, but element {} is {}",
                        who,
                        i,
                        crate::value::with_article(v.type_name())
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
        // Mixed Int/Float: exact via the float's integer part (the widening
        // collapse made min/max permutation-dependent above 2^53). A NaN falls
        // to the total_cmp tail so sort's comparator stays a TOTAL order.
        (Value::Int(x), Value::Float(y)) => {
            match crate::interp::ops::int_float_cmp(*x, *y) {
                Some(o) => o,
                // `int_float_cmp` answers `None` only for a NaN, which `float_order`
                // places last — so a NaN is Greater than any Int.
                None => crate::interp::ops::float_order(*x as f64, *y),
            }
        }
        (Value::Float(y), Value::Int(x)) => {
            match crate::interp::ops::int_float_cmp(*x, *y) {
                Some(o) => o.reverse(),
                None => crate::interp::ops::float_order(*y, *x as f64),
            }
        }
        // `total_cmp`, not `partial_cmp(..).unwrap_or(Equal)`: the old fallback made a
        // `NaN` compare *Equal* to every other value, which is intransitive (`3 == NaN`,
        // `NaN == 1`, yet `3 > 1`). Rust's sort detects such a non-total comparator and
        // *panics* ("comparison function does not implement a total order"), aborting the
        // interpreter on a valid array like `[1.0, sqrt(-1.0), 3.0].sort()`. `total_cmp`
        // is a genuine total order, so `sort`/`argsort` are total and never abort.
        //
        // Every NaN sorts LAST, sign-independently (`ops::float_order`, ADR 0036 policy
        // 6). Until v0.6.0 the comparator was bare `total_cmp`, which orders by SIGN
        // BIT — and the comment here said so, having been corrected once already from
        // a wrong claim that NaN sorted "after `+inf`, as numpy does". It then observed
        // that matching numpy "would mean a comparator that normalizes NaN sign, a
        // semantics change rather than a comment fix", and documented the behaviour
        // instead. That was the right call for a comment and the wrong resting place
        // for the language: the rule was invisible from Helix source, matched no
        // comparable system, and put the same printed value at both ends of one sorted
        // array. ADR 0036 made the semantics change the comment declined to.
        _ => crate::interp::ops::float_order(
            a.as_f64().unwrap_or(f64::NAN),
            b.as_f64().unwrap_or(f64::NAN),
        ),
    }
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

/// Parse the optional `ddof` (delta degrees of freedom) argument of `var`/`std`: no argument →
/// `0` (population, the default), an integer → that `ddof` (`1` = sample / Bessel's correction).
fn parse_ddof(name: &str, args: &[Value], line: usize, col: usize) -> Result<usize, HelixError> {
    match args {
        [] => Ok(0),
        [Value::Int(d)] if *d >= 0 => Ok(*d as usize),
        [Value::Int(d)] => Err(HelixError::new(
            format!("`{name}` ddof must be >= 0, got {d}"),
            line,
            col,
        )),
        [_] => Err(HelixError::new(
            format!("`{name}` ddof must be an integer (0 = population, 1 = sample)"),
            line,
            col,
        )),
        _ => Err(HelixError::new(
            format!("`{name}` takes an optional ddof (0 = population, 1 = sample), got {} arguments", args.len()),
            line,
            col,
        )),
    }
}

/// A `var`/`std` with `ddof` needs strictly more than `ddof` values (else it would divide by a
/// zero or negative count). Raises a precise error instead of returning `inf`/`NaN`.
fn ddof_fits(xs: &[f64], ddof: usize, name: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if xs.len() <= ddof {
        Err(HelixError::new(
            format!("`{name}` with ddof = {ddof} needs more than {ddof} value(s), got {}", xs.len()),
            line,
            col,
        )
        .hint("ddof = 0 (population, the default) divides by n; ddof = 1 (sample) divides by n−1."))
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
    // The compensation is only meaningful while the running sum is finite. Once `sum`
    // is ±inf, the very next `(sum - t)` is `inf - inf` = NaN, so `c` goes NaN and the
    // final `sum + c` turns a CORRECT ±inf into NaN — which is how `[1e308 * 10].sum()`
    // answered NaN where IEEE-754, python3, NumPy and Helix's own `+` all answer inf.
    // A non-finite running sum is already final (it can never return to finite), so
    // return it and drop the compensator. Neumaier is kept everywhere else: it is
    // genuinely more accurate on finite data, and this guard costs one predictable
    // branch at the very end of the loop, not inside it.
    if sum.is_finite() {
        sum + c
    } else {
        sum
    }
}

fn population_std(xs: &[f64]) -> f64 {
    let mean = neumaier_sum(xs) / xs.len() as f64;
    let sq: Vec<f64> = xs.iter().map(|x| (x - mean).powi(2)).collect();
    let var = neumaier_sum(&sq) / xs.len() as f64;
    var.sqrt()
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

/// Parse the single positive-length argument shared by `kmers`/`windows`.
/// The shared upper bound on a user-controllable output element count (matches the
/// `range` cap). Past this, an op errors cleanly rather than OOM-aborting.
pub(crate) const MAX_ELEMENTS: usize = 100_000_000;

/// May a FAILED method call `recv.name(args…)` retry as the builtin `name(recv, args…)`?
///
/// The other half of UFCS, decided where the v0.3.0 parser rewrite could not decide it:
/// on the receiver, at run time. Four receiver kinds never fall back —
///
/// * `PyObject` — its attributes are resolved by Python at run time; no static table
///   sees them, and capturing one silently rewrote `np.round(1.5)` into
///   `round(np, 1.5)`, which type-checks. The bug this predicate exists to prevent.
/// * `Node` falls back only for names the tape does NOT own
///   (`autodiff::is_tape_method`): `v.sum(1)` keeps the tape's arity error, while
///   `v.to_array()` and `v.tan()` retry as the free builtins — which handle a
///   tracked value themselves, so the two spellings can no longer disagree.
/// * `DataFrame` / `GroupBy` — both engines dispatch these BEFORE the shared
///   `call_method`, so a fallback here would fire on one engine and not the other.
///
/// The four kinds that never fall back: PyObject, DataFrame, GroupBy — and, for
/// tape-owned names only, Node.
///
/// Everything else falls back only when its own table does not claim the name — a
/// method that owns its name always wins — and the caller must only consult this
/// AFTER dispatch has failed, which is what makes the whole scheme additive: it
/// substitutes an answer where an error stood.
/// The tracked-fold element gate (ADR 0003): a tracked array fold must accept
/// exactly what the plain fold accepts — numbers, with a tracked SCALAR
/// counting as a number — and must propagate `missing`/NaN exactly as the
/// plain fold does. Without this, wrapping ONE element in `variable(...)`
/// silently granted folds over tensors the plain spelling refuses loudly, and
/// `[tracked, missing].sum()` errored where the plain sum answers `missing`
/// (both found by the stabilization sweep). `Ok(Some(v))` short-circuits with
/// that value; `Ok(None)` means fold away.
pub(super) fn tracked_fold_gate(
    items: &[Value],
    who: &str,
    line: usize,
    col: usize,
) -> Result<Option<Value>, HelixError> {
    if missing_or_nan(items) {
        return Ok(Some(Value::Missing));
    }
    for (i, v) in items.iter().enumerate() {
        let ok = match v {
            Value::Int(_) | Value::Float(_) => true,
            Value::Node(n) => crate::autodiff::node_ndim(n) == 0,
            _ => false,
        };
        if !ok {
            let what = match v {
                Value::Node(_) => "a tracked tensor".to_string(),
                other => crate::value::with_article(other.type_name()),
            };
            return Err(HelixError::new(
                format!("`{who}` needs an array of numbers, but element {i} is {what}"),
                line,
                col,
            ));
        }
    }
    Ok(None)
}

pub(crate) fn ufcs_fallback_applies(recv: &Value, name: &str) -> bool {
    match recv {
        Value::PyObject(_)
        | Value::DataFrame(_)
        | Value::GroupBy(_) => false,
        Value::Node(_) => !crate::autodiff::is_tape_method(name),
        other => !crate::registry::type_owns_method(other.type_name(), name),
    }
}


fn no_args(name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(HelixError::new(
            format!("`{}` takes no arguments, got {}", name, args.len()),
            line,
            col,
        ))
    }
}

fn unknown_method(
    type_name: &str,
    name: &str,
    candidates: &[&str],
    line: usize,
    col: usize,
) -> HelixError {
    let err = HelixError::new(
        format!("a {} has no method `{}`", type_name, name),
        line,
        col,
    );
    match crate::suggest::hint(name, crate::suggest::Site::Method, candidates) {
        Some(h) => err.hint(h),
        // No near-miss: point at the doc command instead of dumping 79 names — a dump
        // is a haystack, `helix doc Array` is an answer. Byte-identical to the checker
        // twin in `types.rs` so the engines cannot drift.
        None => err.hint(format!(
            "no similar method — `helix doc {type_name}` lists all {type_name} methods."
        )),
    }
}

