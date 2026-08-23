//! Value-method dispatch (`call_method`) and the per-type method implementations
//! for arrays, strings, and DNA, plus the shared numeric helpers (Neumaier
//! compensated summation, population standard deviation). These are free
//! functions shared by both the tree-walker and the bytecode VM — the parent
//! module re-exports them, so `crate::interp::call_method` still resolves.

use super::*;
use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;


/// GC fraction of a DNA string, shared by `gc_content`, `at_content`, and `mean_gc` so
/// the three cannot drift. The IUPAC policy lives on `simd::gc_counts`: `S` counts as
/// GC, `W` as non-GC, and the codes ambiguous about GC-ness (`N`, `R Y K M B D H V`)
/// are excluded from numerator and denominator alike. `Ok(None)` means the sequence
/// has no classifiable base — the fraction is unknown, and the caller renders it as
/// `missing` (ADR 0001) rather than a fabricated `0.0`. Errors only on an empty
/// sequence, which is a mistake in the program rather than a condition in the data.
fn dna_gc(s: &str, who: &str, line: usize, col: usize) -> Result<Option<f64>, HelixError> {
    if s.is_empty() {
        return Err(HelixError::new(
            format!("cannot compute `{who}` of an empty sequence"),
            line,
            col,
        ));
    }
    // `Dna` is ASCII (validated + upper-cased at construction), so count raw bytes —
    // AVX2 when available, else the auto-vectorized scalar path.
    let (gc, classified) = crate::simd::gc_counts(s.as_bytes());
    Ok((classified > 0).then(|| gc as f64 / classified as f64))
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
    // The cookie-jar handle answers a small set of its own methods; everything else is
    // a server/stream method below.
    if let crate::serve::NetHandle::CookieJar(jar) = &**h {
        let now = crate::cookiejar::now_unix();
        return match name {
            "cookies" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`cookies` takes no arguments", line, col));
                }
                // Each cookie as a record `{name, value, domain, path}`, in jar order.
                let rows = jar.snapshot(now).into_iter().map(|(n, v, d, p)| {
                    Value::Record(Rc::new(vec![
                        (crate::symbol::Symbol::intern("name"), Value::Str(Rc::new(n))),
                        (crate::symbol::Symbol::intern("value"), Value::Str(Rc::new(v))),
                        (crate::symbol::Symbol::intern("domain"), Value::Str(Rc::new(d))),
                        (crate::symbol::Symbol::intern("path"), Value::Str(Rc::new(p))),
                    ]))
                });
                Ok(Value::array(rows.collect()))
            }
            "count" | "length" => {
                if !args.is_empty() {
                    return Err(HelixError::new(format!("`{name}` takes no arguments"), line, col));
                }
                Ok(Value::Int(jar.snapshot(now).len() as i64))
            }
            "clear" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`clear` takes no arguments", line, col));
                }
                jar.clear();
                Ok(Value::Unit)
            }
            other => Err(HelixError::new(
                format!("a cookie jar has no method `{other}`"),
                line,
                col,
            )
            .hint("jar methods: cookies, count, clear.")),
        };
    }

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
        // `expect(k)` → the value, RAISING on absence — the loud companion to `get`.
        // ADR 0001 keeps `get`/`d[k]` answering `missing` (an absent value is a condition
        // in the data); `expect` is for when absence would be a mistake in the PROGRAM,
        // and it raises at the lookup — before a `missing` is minted and laundered
        // through arithmetic into a number-shaped hole nothing downstream can trace.
        "expect" => {
            arity(1)?;
            let k = key_of(&args[0])?;
            match map.get(&k) {
                Some(v) => Ok(v.clone()),
                None => {
                    let e = HelixError::new(
                        format!(
                            "key `{}` not found in this dict ({} key{})",
                            args[0],
                            map.len(),
                            if map.len() == 1 { "" } else { "s" }
                        ),
                        line,
                        col,
                    );
                    // One-edit did-you-mean over the dict's OWN string keys — the house
                    // policy (a wrong suggestion is worse than silence). Non-string keys
                    // can't typo by spelling, so they never suggest.
                    let near = match &args[0] {
                        Value::Str(want) => map
                            .keys()
                            .filter_map(|c| match c.to_value() {
                                Value::Str(s) => crate::error::typo_distance(want, &s)
                                    .map(|d| (d, (*s).clone())),
                                _ => None,
                            })
                            .min_by_key(|(d, _)| *d)
                            .map(|(_, s)| s),
                        _ => None,
                    };
                    Err(match near {
                        Some(s) => e.hint(format!("did you mean `{s}`?")),
                        None => e.hint(
                            "`.has(k)` checks presence; `.get(k)` answers `missing` \
                             instead of raising, so `.get(k) ?? default` supplies a \
                             fallback.",
                        ),
                    })
                }
            }
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
        // `expect(k)` → the field's value, RAISING on absence — the record twin of the
        // dict arm above: the loud lookup for when a missing field means the PROGRAM is
        // wrong, raising before a `missing` is minted (ADR 0001's propagating default
        // stays on `get` and static access).
        "expect" => {
            arity(1)?;
            let k = key(&args[0])?;
            match fields.iter().find(|(s, _)| s.as_str() == k) {
                Some((_, v)) => Ok(v.clone()),
                None => {
                    let e = HelixError::new(
                        format!(
                            "field `{k}` not found in this record ({} field{})",
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" }
                        ),
                        line,
                        col,
                    );
                    let near = fields
                        .iter()
                        .filter_map(|(s, _)| {
                            crate::error::typo_distance(&k, s.as_str())
                                .map(|d| (d, s.as_str().to_string()))
                        })
                        .min_by_key(|(d, _)| *d)
                        .map(|(_, s)| s);
                    Err(match near {
                        Some(s) => e.hint(format!("did you mean `{s}`?")),
                        None => e.hint(
                            "`.has(k)` checks presence; `.get(k)` answers `missing` \
                             instead of raising, so `.get(k, default)` supplies a \
                             fallback.",
                        ),
                    })
                }
            }
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
        // The name is a FIELD of this record, not one of the five dynamic-access methods.
        // The generic "no method" help (`get`/`has`/`keys`/`values`/`items`) is useless
        // here and actively misleading: none of them is what the author wanted. The
        // object-API spelling `r.go(3)` is what everyone writes first, and every working
        // alternative — `(r.go)(3)`, `f = r.go`, `r["go"](3)` — went unmentioned.
        _ => match fields.iter().find(|(s, _)| s.as_str() == name) {
            Some((_, held)) => {
                let e = HelixError::new(
                    format!("`{name}` is a field of this record, not a method"),
                    line,
                    col,
                );
                Err(if held.type_name() == "Function" {
                    e.hint(format!(
                        "it holds a function, so call it through the field: `(rec.{name})(…)` \
                         — or bind it first, `f = rec.{name}`, then `f(…)`."
                    ))
                } else {
                    e.hint(format!(
                        "read it without parentheses: `rec.{name}` (it holds {}).",
                        crate::value::with_article(held.type_name())
                    ))
                })
            }
            None => Err(unknown_method(
                "Record",
                name,
                &crate::registry::methods_of(crate::registry::RECORD_METHODS),
                line,
                col,
            )),
        },
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
        // `total_cmp`, not `partial_cmp(..).unwrap_or(Equal)`: the old fallback made a
        // `NaN` compare *Equal* to every other value, which is intransitive (`3 == NaN`,
        // `NaN == 1`, yet `3 > 1`). Rust's sort detects such a non-total comparator and
        // *panics* ("comparison function does not implement a total order"), aborting the
        // interpreter on a valid array like `[1.0, sqrt(-1.0), 3.0].sort()`. `total_cmp`
        // is a genuine total order, so `sort`/`argsort` are total and never abort.
        //
        // Where a `NaN` lands is decided by its SIGN BIT, which is worth stating plainly
        // because an earlier version of this comment claimed "after `+inf`, as numpy
        // does" and named `sqrt(-1.0)` as the example — and that example sorts to the
        // FRONT, because the NaN it produces has its sign bit set. Only a positive NaN
        // sorts last. numpy puts every NaN last regardless of sign, so this does NOT
        // match numpy; matching it would mean a comparator that normalizes NaN sign, a
        // semantics change rather than a comment fix. Documented as it behaves.
        // Reductions
        // (`min`/`max`/`median`) filter `NaN` to `missing` before comparing, so they are
        // unaffected; this only changes where a `NaN` lands in a *sorted* result.
        _ => a.as_f64().unwrap_or(f64::NAN).total_cmp(&b.as_f64().unwrap_or(f64::NAN)),
    }
}

/// Numeric-reduction fast path for **typed** arrays (`Ints`/`Floats`): read the
/// packed buffer directly, never materializing a `Vec<Value>`. Returns `Ok(None)`
/// for a `Values` array, a non-reduction method, an argument-bearing call, or a
/// `Float` array containing `NaN` — so the caller's general, missing/NaN-aware path
/// runs and the result matches the untyped array exactly. Typed arrays are
/// missing-free by construction, so no missing check is needed here.
/// The index of the largest (`want_max`) or smallest element of a PACKED array, or `None`
/// to decline — which sends `argmin`/`argmax` back to the tuple reduce their desugar
/// produces, so a declined shape keeps its exact error text and caret column.
///
/// This is the whole of the `argmin`/`argmax` fix: those methods never reach runtime as
/// names (they are rewritten at parse time into `enumerate` + a reduce over tuples), so the
/// only way to give them a kernel is to give the desugar a verb that DOES reach here.
///
/// THE COMPARISON RULES DIFFER BY TYPE AND THAT IS DELIBERATE. They mirror
/// [`crate::interp::ops`]'s `<`/`>`, which is what the reduce this replaces actually
/// evaluates — NOT `total_cmp`, which every neighbouring packed arm (`sort`, `argsort`,
/// `min`, `max`) uses:
///
/// * Ints compare with exact `i64` ordering. `total_cmp` on an `as f64` cast would lose
///   precision above 2^53, and a comment in `ops.rs` records that exact bug.
/// * Floats compare with IEEE `>`/`<`, under which `0.0` and `-0.0` are EQUAL. So
///   `[0.0, -0.0].argmin()` and `[-0.0, 0.0].argmin()` both answer 0 — first-wins keeps
///   index 0 either way. A `total_cmp` kernel would order the zeros and answer 1 for two
///   of those four shapes, silently changing results under a performance commit. Whether
///   that IEEE answer is the right one is a separate, recorded, open question; reproducing
///   it is this change's job.
/// * ANY NaN declines. `[sqrt(-1.0)].argmax()` — one element, no comparison to make —
///   still raises "cannot compare these values (NaN?)" today, because the reduce compares
///   the seed against the first element. Note the opposite convention two arms above:
///   packed `sort`/`argsort` deliberately do NOT defer on NaN, because they have NaN
///   *placement* semantics rather than a raise.
///
/// Ties are first-wins (`[2,2,2].argmax()` → 0), so the scan must update only on a STRICT
/// improvement. A range needs no comparisons at all: it is strictly monotonic (a zero step
/// is rejected at construction), so the answer is an endpoint. Every empty array declines —
/// the empty error belongs to the reduce seed, and only the reduce can say it with the
/// right column.
fn packed_arg_extreme(ad: &crate::value::ArrayData, want_max: bool) -> Option<i64> {
    use crate::value::ArrayData;
    fn scan<T: Copy>(xs: &[T], better: impl Fn(T, T) -> bool) -> i64 {
        let mut best = 0usize;
        for i in 1..xs.len() {
            if better(xs[i], xs[best]) {
                best = i;
            }
        }
        best as i64
    }
    match ad {
        // Boxed and lazy-pair arrays keep the general path: their elements are `Value`s,
        // so there is no packed buffer to scan and nothing to win.
        ArrayData::Values(_) | ArrayData::Enumerate { .. } => None,
        ArrayData::Ints(xs) if !xs.is_empty() => Some(if want_max {
            scan(xs, |a, b| a > b)
        } else {
            scan(xs, |a, b| a < b)
        }),
        ArrayData::Floats(xs) if !xs.is_empty() && !xs.iter().any(|f| f.is_nan()) => {
            Some(if want_max { scan(xs, |a, b| a > b) } else { scan(xs, |a, b| a < b) })
        }
        ArrayData::Range { step, len, .. } if *len > 0 => {
            Some(if (*step > 0) == want_max { *len as i64 - 1 } else { 0 })
        }
        // Empties (in every representation) and float arrays holding a NaN.
        _ => None,
    }
}

fn array_numeric_fast(
    ad: &crate::value::ArrayData,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Option<Value>, HelixError> {
    use crate::value::ArrayData;
    // Lazy-Range `take`/`drop`: O(1) re-slicing of the arithmetic progression — the
    // whole point of the lazy representation (`range(100000000).take(1)` previously
    // materialized ~1.6 GB of boxed Values to keep one element, contradicting the
    // documented O(1)). Int counts only, mirroring the general path exactly (negative
    // clamps to 0, over-take/-drop clamps to the length); float/missing counts defer
    // so the general path's errors stay identical. A fully-dropped range keeps an
    // empty representation rather than computing `start + step*len`, which the Range
    // invariant does not guarantee to fit i64.
    if let ("take" | "drop", [Value::Int(n)], ArrayData::Range { start, step, len }) =
        (name, args, ad)
    {
        let k = (*n).max(0).min(*len as i64) as usize;
        return Ok(Some(if name == "take" {
            Value::lazy_range(*start, *step, k)
        } else if k >= *len {
            Value::lazy_range(0, 1, 0)
        } else {
            // Element k is in range by the Range invariant, so the i128 math fits i64.
            Value::lazy_range(
                (*start as i128 + *step as i128 * k as i128) as i64,
                *step,
                *len - k,
            )
        }));
    }
    // The same re-slice for a PACKED numeric array. The lazy-`Range` arm above was added
    // when `range(100000000).take(1)` was found materializing ~1.6 GB to keep one element —
    // but a range that has been through a `map` is `Ints`/`Floats`, not `Range`, and that
    // spelling kept boxing the whole source: `(0..20_000_000).map(it * 2).take(3)` cost
    // 503 MB against 190 MB for the array alone, i.e. 320 MB of `Vec<Value>` to keep three
    // numbers. One defect, fixed for one representation and not its neighbour.
    //
    // Counts are clamped exactly as the arm above and the general path do (negative → 0,
    // over-take/-drop → the length), and a non-`Int` count defers so the general path's
    // errors stay identical.
    if let ("take" | "drop", [Value::Int(n)]) = (name, args) {
        let slice_ints = |v: &Vec<i64>| -> Value {
            let k = (*n).max(0).min(v.len() as i64) as usize;
            let part = if name == "take" { &v[..k] } else { &v[k..] };
            Value::int_array(part.to_vec())
        };
        let slice_floats = |v: &Vec<f64>| -> Value {
            let k = (*n).max(0).min(v.len() as i64) as usize;
            let part = if name == "take" { &v[..k] } else { &v[k..] };
            Value::float_array(part.to_vec())
        };
        match ad {
            ArrayData::Ints(v) => return Ok(Some(slice_ints(v))),
            ArrayData::Floats(v) => return Ok(Some(slice_floats(v))),
            _ => {}
        }
    }
    // `contains(v)` / `index_of(v)` answer a SCALAR, but both boxed the entire source to
    // do it: `(0..20_000_000).map(it * 2).contains(4)` cost 491 MB against 185 MB for the
    // array alone — 306 MB of `Vec<Value>` built to settle a question decided by element
    // 2. Their closure-taking neighbours `any(p)` / `position(p)` already stream and cost
    // nothing extra, so this is one operation with two spellings where only one was fixed:
    // the same shape as `take`/`drop` (packed vs lazy `Range`), `clamp` (array vs scalar),
    // `dot` (vs `sum`/`cumsum`), and duplicate record fields (literal vs update).
    //
    // The scan calls the SAME `values_equal` on the SAME `Value` the general path would
    // have built — `to_values()` is `(0..len).map(get)` for every non-`Values`
    // representation — one stack temporary at a time instead of a heap `Vec` of them. So
    // cross-type equality (`1 == 1.0`, Rational-vs-Int), `missing` identity equality and
    // `NaN != NaN` all stay exactly as they were by construction, not by re-derivation;
    // re-deriving them here is precisely how the second spelling drifts from the first.
    //
    // `Values` arrays already hold their `Value`s, so they defer — there is nothing to
    // avoid materializing, and the general path's `any`/`position` are the same scan.
    // A wrong arity also defers, so both methods' (differing) arity errors are untouched.
    if let ("contains" | "index_of", [needle]) = (name, args)
        && !matches!(ad, ArrayData::Values(_))
    {
        let hit =
            (0..ad.len()).position(|i| crate::interp::ops::values_equal(&ad.get(i), needle));
        return Ok(Some(match (name, hit) {
            ("contains", h) => Value::Bool(h.is_some()),
            (_, Some(i)) => Value::Int(i as i64),
            (_, None) => Value::Missing,
        }));
    }
    if !args.is_empty() {
        return Ok(None);
    }
    // Cheap length/positional methods read the packed buffer directly, so they don't
    // box every element into a `Value` the way `to_values()` would — e.g.
    // `range(1_000_000).first()` returns element 0 without materializing a million
    // Values. Byte-identical to the general path (`length`==`count`==len; `first`/`last`
    // are Missing on empty else the element).
    match name {
        "count" | "length" => {
            return Ok(match ad {
                ArrayData::Values(_) => None,
                ArrayData::Ints(xs) => Some(Value::Int(xs.len() as i64)),
                ArrayData::Floats(xs) => Some(Value::Int(xs.len() as i64)),
                // O(1) on the lazy representations — a lazy enumerate's count is its
                // inner length (previously it materialized every (index, element)
                // tuple: ~1 GB for `range(10000000).enumerate().count()`).
                ArrayData::Range { len, .. } => Some(Value::Int(*len as i64)),
                ArrayData::Enumerate { inner } => Some(Value::Int(inner.len() as i64)),
                // The FROZEN len (see the variant's invariant 1) — never a recomputed
                // `min(a.len(), b.len())`, which is exponential on `z = z.zip(z)`. This is
                // the line that turns `a.zip(a).length()` on 5M from 631 MB into ~16 MB.
                ArrayData::Zip { len, .. } => Some(Value::Int(*len as i64)),
            });
        }
        "first" | "last" => {
            let first = name == "first";
            return Ok(match ad {
                ArrayData::Values(_) => None,
                // O(1): one (index, element) tuple on demand; Missing on empty like
                // the general path.
                ArrayData::Enumerate { inner } => Some(if inner.is_empty() {
                    Value::Missing
                } else {
                    ad.get(if first { 0 } else { inner.len() - 1 })
                }),
                // READ THE STORED `len`, NOT `a.len()`. On `[1,2,3,4].zip([10,20])` those are
                // 4 and 2; indexing at 3 would read `b[3]` out of bounds and abort the
                // runtime — which ADR-0024 forbids outright.
                ArrayData::Zip { len, .. } => Some(if *len == 0 {
                    Value::Missing
                } else {
                    ad.get(if first { 0 } else { *len - 1 })
                }),
                ArrayData::Ints(xs) => Some(if xs.is_empty() {
                    Value::Missing
                } else {
                    Value::Int(if first { xs[0] } else { xs[xs.len() - 1] })
                }),
                ArrayData::Floats(xs) => Some(if xs.is_empty() {
                    Value::Missing
                } else {
                    Value::Float(if first { xs[0] } else { xs[xs.len() - 1] })
                }),
                // O(1) — `range(20M).first()` computes one element, no 160 MB materialization.
                ArrayData::Range { len, .. } if *len == 0 => Some(Value::Missing),
                ArrayData::Range { .. } => Some(ad.get(if first { 0 } else { ad.len() - 1 })),
            });
        }
        // `sort`/`reverse` on a packed array stayed packed nowhere: both built a
        // `Vec<Value>` of the whole source AND returned it through `Value::array`, which
        // (unlike `Value::array_sniff`) does not re-pack. So the cost was paid twice —
        // 320 MB of boxing to do the work, and a permanently boxed result that silently
        // stripped the fast path from everything downstream:
        //
        //     xs = (0..20000000).map(it * 2)
        //     xs.reverse().first()   797 MB  ->  346 MB
        //     xs.sort().first()      799 MB  ->  346 MB
        //
        // Sorting the packed buffer is also the exact comparison `numeric_cmp` performs:
        // two `Int`s compare as `i64` (deliberately NOT via `f64`, which collapses values
        // above 2^53), and anything else via `total_cmp`. `sort_unstable` is safe here
        // where `sort_by` was stable, because elements that compare equal under either
        // order are indistinguishable — equal `i64`s, and `total_cmp`-equal `f64`s are
        // bit-identical (`-0.0` and `0.0` are NOT equal under `total_cmp`, so even signed
        // zeros keep a deterministic position).
        //
        // `Values` defers (nothing to unbox) and so does `Enumerate`, whose elements are
        // tuples — `sort` rejects those, and that rejection is the general path's to make.
        "sort" | "reverse" => {
            let rev = name == "reverse";
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                ArrayData::Ints(xs) => {
                    let mut v = xs.clone();
                    if rev {
                        v.reverse();
                    } else {
                        v.sort_unstable();
                    }
                    Some(Value::int_array(v))
                }
                ArrayData::Floats(xs) => {
                    let mut v = xs.clone();
                    if rev {
                        v.reverse();
                    } else {
                        v.sort_unstable_by(f64::total_cmp);
                    }
                    Some(Value::float_array(v))
                }
                // Reversing an arithmetic progression is another progression, so this is
                // O(1) and stays lazy: `range(100000000).reverse().first()` allocates
                // nothing. The last element is in range by the Range invariant, so the
                // i128 start math fits i64.
                //
                // `wrapping_neg`, and it is exact rather than a shrug. The only step it
                // does not negate cleanly is `i64::MIN`, which it returns unchanged — and
                // that is the right step anyway, because `-2^63 == 2^63 (mod 2^64)`, so
                // adding it and subtracting it are the same operation in the i128-then-
                // truncate arithmetic `get` uses. Such a range also holds at most two
                // elements (`start + 2*i64::MIN` cannot fit), so only i=0 and i=1 are ever
                // evaluated, where the equivalence is exact.
                //
                // This started life as a `checked_neg` with a materializing fallback for
                // the overflow. Sabotaging it to `wrapping_neg` SURVIVED the test — and
                // the reason was not a weak test but the equivalence above: the fallback
                // could never produce a different answer. A guard whose removal breaks
                // nothing is decoration, so it is gone. The two-element `i64::MIN`-step
                // case is pinned in the test regardless, since that is the behaviour
                // being relied on here.
                //
                // SORTING one is O(1) for the same reason: a range is monotonic, so its
                // sorted form is either the range itself (ascending step) or exactly that
                // reverse (descending step). A zero step is rejected at construction
                // ("`range` step must not be zero"), so there is no third case. Writing it
                // this way also removed the last `unwrap` here — materializing the range
                // to sort it needed `to_ints().unwrap()`, which the ADR-0024 never-abort
                // ratchet rightly refused; the better code has nothing to unwrap.
                ArrayData::Range { start, step, len } if *len > 0 => Some(if rev || *step < 0 {
                    Value::lazy_range(
                        (*start as i128 + *step as i128 * (*len as i128 - 1)) as i64,
                        step.wrapping_neg(),
                        *len,
                    )
                } else {
                    Value::lazy_range(*start, *step, *len)
                }),
                // An empty range both sorts and reverses to itself.
                ArrayData::Range { .. } => Some(Value::lazy_range(0, 1, 0)),
            });
        }
        // `argsort` sorts INDICES by the values they point at, so the general path paid
        // twice: `to_values()` boxed the whole column (16 B/element), then every
        // comparison chased a `Value` enum through two random-access derefs. The packed
        // arm sorts the same indices against the raw buffer.
        //
        // STABILITY IS OBSERVABLE HERE, unlike `sort`: the output is indices, so equal
        // keys must keep their original order exactly as the general path's stable
        // `sort_by` does. `sort_unstable_by` + a `.then(a.cmp(&b))` index tie-break
        // reproduces a stable sort exactly (no two index pairs ever compare Equal).
        //
        // Floats compare by `total_cmp`, which is what `numeric_cmp` does for two floats
        // — so NaN (by sign bit) and signed zeros land exactly where the general path
        // puts them, and no NaN deferral is needed or wanted (argsort, like `sort`, has
        // NaN placement semantics rather than a `missing` answer).
        //
        // A RANGE needs no comparisons at all: it is strictly monotonic (zero step is
        // rejected at construction), so its argsort is the identity permutation for an
        // ascending step and the reversal for a descending one — both lazy, O(1).
        "argsort" => {
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                ArrayData::Ints(xs) => {
                    let mut idx: Vec<i64> = (0..xs.len() as i64).collect();
                    idx.sort_unstable_by(|&a, &b| {
                        xs[a as usize].cmp(&xs[b as usize]).then(a.cmp(&b))
                    });
                    Some(Value::int_array(idx))
                }
                ArrayData::Floats(xs) => {
                    let mut idx: Vec<i64> = (0..xs.len() as i64).collect();
                    idx.sort_unstable_by(|&a, &b| {
                        xs[a as usize].total_cmp(&xs[b as usize]).then(a.cmp(&b))
                    });
                    Some(Value::int_array(idx))
                }
                ArrayData::Range { step, len, .. } => Some(if *step > 0 || *len == 0 {
                    Value::lazy_range(0, 1, *len)
                } else {
                    Value::lazy_range(*len as i64 - 1, -1, *len)
                }),
            });
        }
        // `cumsum` already RETURNED a packed column; what it never had was a packed
        // INPUT, so the source was boxed into a `Vec<Value>` before it was even called:
        // `(0..20000000).map(it * 2).cumsum().last()` cost 645 MB against 186 MB for the
        // array alone. Same class as `sort`/`reverse`, opposite end of the pipe.
        //
        // The accumulation is deliberately identical to the general path rather than
        // better: `wrapping_add` for ints, and plain `+=` for floats — NOT the Neumaier
        // summation `sum`/`mean` use, which would give a different (more accurate) result
        // and break the two paths apart. `cumsum` also checks only for `missing`, never
        // NaN, so unlike the numeric reductions this must NOT defer on a NaN — a NaN
        // simply poisons the running total from that point on, exactly as before.
        "cumsum" => {
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                // `to_ints` borrows for `Ints` and computes for `Range`; `None` cannot
                // happen for either, and deferring is the safe reading of it regardless.
                ArrayData::Ints(_) | ArrayData::Range { .. } => match ad.to_ints() {
                    Some(xs) => {
                        let mut acc = 0i64;
                        Some(Value::int_array(
                            xs.iter()
                                .map(|&i| {
                                    acc = acc.wrapping_add(i);
                                    acc
                                })
                                .collect(),
                        ))
                    }
                    None => None,
                },
                ArrayData::Floats(xs) => {
                    let mut acc = 0.0;
                    Some(Value::float_array(
                        xs.iter()
                            .map(|&x| {
                                acc += x;
                                acc
                            })
                            .collect(),
                    ))
                }
            });
        }
        _ => {}
    }
    if !matches!(
        name,
        "sum" | "mean" | "std" | "var" | "median" | "min" | "max"
    ) {
        return Ok(None);
    }
    match ad {
        ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => Ok(None),
        ArrayData::Ints(xs) => array_int_reduce(xs, name, line, col).map(Some),
        // A reduction consumes every element, so materialize the range once (bit-identical to
        // reducing the equivalent `Int` array); still lazy for the O(1) methods above.
        ArrayData::Range { .. } => {
            array_int_reduce(&ad.to_ints().unwrap(), name, line, col).map(Some)
        }
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
            // `total_cmp`, NOT IEEE `<`/`>` — the same comparison the boxed path's
            // `numeric_cmp` makes for floats. Under IEEE, `-0.0` and `0.0` compare EQUAL,
            // so a first-wins scan returns whichever zero came first and the SAME array
            // answered differently depending on its representation:
            //
            //     [0.0, -0.0].min()          was  0.0   (packed: IEEE tie, first wins)
            //     [0.0, -0.0][0:2].min()          -0.0  (boxed: total_cmp, no tie)
            //     [0.0, -0.0].sort().first()      -0.0
            //
            // — and packed `min` was not even permutation-invariant ([-0.0, 0.0].min()
            // was -0.0). Under `total_cmp` the zeros are ORDERED (-0.0 < 0.0), so min is
            // -0.0 and max is 0.0 regardless of order and of representation, and
            // `min() == sort().first()` / `max() == sort().last()` hold everywhere. For
            // every pair of distinct non-zero values `total_cmp` agrees with IEEE `<`, so
            // nothing else moves. A NaN never reaches here — the caller defers any
            // NaN-containing array to the general path, which yields `missing` (ADR 0001).
            let mut best = xs[0];
            for &x in &xs[1..] {
                let ord = x.total_cmp(&best);
                if (name == "min" && ord == std::cmp::Ordering::Less)
                    || (name == "max" && ord == std::cmp::Ordering::Greater)
                {
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
            // TRACKED elements: fold-add on the tape, divide by the count — division
            // is differentiable, so `.mean()` carries gradients exactly as its
            // reduce-then-divide spelling does. Same ADR-0003 rule as `.sum()`: the
            // spellings of one concept must not fork by capability (the v0.2.5 field
            // re-verification found `.sum()` closed and this one still open).
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(0));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Add, &acc, v, line, col)?;
                }
                return crate::autodiff::binary(
                    &crate::ast::BinOp::Div,
                    &acc,
                    &Value::Float(items.len() as f64),
                    line,
                    col,
                );
            }
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            // Optional `ddof`: `std()` = population (÷n, default), `std(1)` = sample (÷n−1).
            let ddof = parse_ddof(name, args, line, col)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            ddof_fits(&xs, ddof, "std", line, col)?;
            // ddof == 0 keeps the exact existing population path (bit-identical to before).
            let v = if ddof == 0 {
                population_std(&xs)
            } else {
                crate::stats::variance_ddof(&xs, ddof).sqrt()
            };
            Ok(Value::Float(v))
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
            // Optional `ddof`: `var()` = population (÷n, default), `var(1)` = sample (÷n−1).
            let ddof = parse_ddof(name, args, line, col)?;
            if missing_or_nan(items) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "var", line, col)?;
            empty_guard(&xs, "var", line, col)?;
            ddof_fits(&xs, ddof, "var", line, col)?;
            // ddof == 0 keeps the exact existing population path (bit-identical to before).
            let v = if ddof == 0 {
                crate::stats::variance(&xs)
            } else {
                crate::stats::variance_ddof(&xs, ddof)
            };
            Ok(Value::Float(v))
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
            // TRACKED elements fold on the tape — left-to-right adds, exactly what the
            // reduce spelling produces — so `.sum()` and the fold carry gradients
            // alike. Before this, the two spellings of one concept silently forked by
            // CAPABILITY (ADR 0003's wound, found by the nn field report): the fold
            // differentiated while `.sum()` errored, forcing every dot product and
            // loss into hand-written folds.
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(0));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Add, &acc, v, line, col)?;
                }
                return Ok(acc);
            }
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
            // A tracked element: fold with the differentiable max/min. The fold
            // plus the ties-to-first rule means the FIRST extreme element gets
            // the gradient — deterministic, and consistent with the scalar pair.
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                let mut acc = items[0].clone();
                for v in &items[1..] {
                    acc = crate::autodiff::binary_builtin(name, &acc, v, line, col)?;
                }
                return Ok(acc);
            }
            // WIDENED TO `sort`'s DOMAIN — ADR 0025 (b), option b1: all numbers, all
            // strings, or all DNA, each ordered by the comparator `sort` uses for that
            // type. `min`/`max` were the one ordering spelling still numbers-only, so
            // `["b","a"].min()` errored while `["b","a"].min_by(it)` answered "a" and
            // `["b","a"].sort()` answered ["a","b"] — three spellings, two domains.
            //
            // The REDUCTION policy is unchanged and is the part that stays different from
            // `sort` on purpose: an array containing `missing` (or NaN) reduces to
            // `missing` (checked above, now covering the widened types too), where sorting
            // REFUSES — reducing and ordering-in-place are different questions (ADR 0001).
            // The empty array still errors, via `empty_guard` on the numeric branch —
            // `.all()` is vacuously true on empty, so empty arrays take that branch, as
            // before.
            //
            // The selector below is `sort().first()`/`sort().last()` in one pass:
            // first-wins on ties, which for numerics under `total_cmp` cannot differ from
            // `sort`'s stable order, and for Str/Dna ties means equal `Rc` contents.
            let pick = |cmp: &dyn Fn(&Value, &Value) -> std::cmp::Ordering| {
                let mut best_idx = 0;
                for i in 1..items.len() {
                    let ord = cmp(&items[i], &items[best_idx]);
                    let better = if name == "min" {
                        ord == std::cmp::Ordering::Less
                    } else {
                        ord == std::cmp::Ordering::Greater
                    };
                    if better {
                        best_idx = i;
                    }
                }
                items[best_idx].clone()
            };
            if items.iter().all(|v| v.as_f64().is_some()) {
                // Numeric (or empty). `numeric_cmp` compares the original `Value`s EXACTLY
                // — not their f64 widening, which would collapse two i64 above 2^53 to the
                // same value and pick the wrong element (and disagree with the packed Int
                // path). `numeric_vec` cannot fail here; it powers `empty_guard`.
                let xs = numeric_vec(items, name, line, col)?;
                empty_guard(&xs, name, line, col)?;
                Ok(pick(&|a, b| numeric_cmp(a, b)))
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                Ok(pick(&|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                }))
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                Ok(pick(&|a, b| match (a, b) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                }))
            } else {
                // `sort`'s domain wording, with this method's name — one concept, one
                // message (the old text named numbers only and pointed at the first
                // offending element, which is now often a legal type in the wrong mix).
                Err(HelixError::new(
                    format!("`{name}` needs an array of all numbers, all strings, or all DNA"),
                    line,
                    col,
                ))
            }
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
            } else if items.iter().any(|v| matches!(v, Value::Missing)) {
                // Name the actual blocker: every present value may well be
                // sortable — it's the `missing` that has no order (ADR 0001
                // makes dropping them an explicit, visible step).
                return Err(HelixError::new(
                    "cannot sort: the array has missing values",
                    line,
                    col,
                )
                .hint("drop them explicitly first: `xs.drop_missing().sort()`."));
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
                        format!("`zip` needs an array, but got {}", crate::value::with_article(v.type_name())),
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
                // A pair is a pair however it is written. `(k, v)` is the canonical
                // spelling, but a two-element ARRAY is what a table transcribed from
                // JSON, a CSV, or a reference document looks like, and refusing it
                // sent people to `reduce(dict(), (d, kv) => d.insert(kv[0], kv[1]))` —
                // a fold standing in for a literal, seventeen times in one corpus.
                // The ARITY still has to be two: a three-element row is a mistake, not
                // a pair, and silently taking its first two would be worse than saying so.
                let pair: Option<Vec<Value>> = match item {
                    Value::Tuple(t) if t.len() == 2 => Some(t.to_vec()),
                    Value::Array(a) if a.len() == 2 => Some(vec![a.get(0), a.get(1)]),
                    _ => None,
                };
                let pair = match pair {
                    Some(p) => p,
                    None => {
                        let what = match item {
                            Value::Tuple(t) => format!("a {}-element tuple", t.len()),
                            Value::Array(a) => format!("a {}-element array", a.len()),
                            other => crate::value::with_article(other.type_name()).to_string(),
                        };
                        return Err(HelixError::new(
                            format!("`to_dict` needs (key, value) pairs, but element {i} is {what}"),
                            line,
                            col,
                        )
                        .hint("each element must hold exactly two values — `[(\"a\", 1), (\"b\", 2)]` or `[[\"a\", 1], [\"b\", 2]]`."));
                    }
                };
                let key = DictKey::from_value(&pair[0]).map_err(|m| HelixError::new(m, line, col))?;
                map.insert(key, pair[1].clone());
            }
            Ok(Value::Dict(Rc::new(map)))
        }
        "unique" => {
            // Distinct values in first-seen order. Text and `Int`/`missing` arrays are
            // O(n) on the same keys `frequencies` uses — the two operations report the
            // same identities by construction, so `xs.unique().length()` can never
            // disagree with `xs.frequencies().length()`. Anything else keeps the O(n^2)
            // `values_equal` scan, which is the only thing that can express the
            // cross-type numeric collapse (`1 == 1.0`); see `IntKey`.
            no_args(name)?;
            let out: Vec<Value> = if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_)))
            {
                // Borrowed keys: the old `HashSet<String>` minted a fresh `String` for
                // every element just to probe the set — 5M allocations to find 10k
                // distinct words.
                let mut seen: std::collections::HashSet<(bool, &str)> =
                    std::collections::HashSet::new();
                items.iter().filter(|v| seen.insert(text_key(v))).cloned().collect()
            } else if items.iter().all(|v| matches!(v, Value::Int(_) | Value::Missing)) {
                // `range(50_000).unique()` was ~1.25 billion `values_equal` comparisons.
                let mut seen: std::collections::HashSet<IntKey> = std::collections::HashSet::new();
                items.iter().filter(|v| seen.insert(int_key(v))).cloned().collect()
            } else if items.iter().all(|v| matches!(v, Value::Float(_) | Value::Missing)) {
                // Same key as `frequencies` uses, so the two cannot disagree on how many
                // identities a float array has. A NaN has NO key — it is equal to nothing,
                // not even itself — so every NaN survives `unique`, which is what the
                // `values_equal` scan did.
                let mut seen: std::collections::HashSet<FloatKey> =
                    std::collections::HashSet::new();
                items
                    .iter()
                    .filter(|v| float_key(v).is_none_or(|k| seen.insert(k)))
                    .cloned()
                    .collect()
            } else {
                let mut out: Vec<Value> = Vec::new();
                for v in items.iter() {
                    if !out.iter().any(|u| values_equal(u, v)) {
                        out.push(v.clone());
                    }
                }
                out
            };
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
                                "`concat` expects arrays, but argument {} is {}",
                                k + 1,
                                crate::value::with_article(other.type_name())
                            ),
                            line,
                            col,
                        ))
                    }
                }
            }
            Ok(Value::array_sniff(out))
        }
        // Every consecutive run of `n` elements, overlapping — the sliding window
        // signal processing and k-mer scanning both want. `Dna` has had this since
        // the bio work; an array had to hand-roll
        // `range(0, len - n + 1).map(i => xs.drop(i).take(n))`, which allocates two
        // intermediate arrays per window. Shorter than `n` yields `[]`, the same
        // answer `Dna.windows` gives, so the two read alike.
        "windows" | "chunks" => {
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`{name}` expects 1 argument, got {}", args.len()),
                    line,
                    col,
                ));
            }
            let n = as_int(&args[0], name, line, col)?;
            if n <= 0 {
                return Err(HelixError::new(
                    format!("`{}` needs a positive size, got {}", name, n),
                    line,
                    col,
                )
                .hint("the window size counts elements, so it must be at least 1."));
            }
            let n = n as usize;
            let mut out: Vec<Value> = Vec::new();
            if name == "windows" {
                if n <= items.len() {
                    let count = items.len() - n + 1;
                    window_count_guard("windows", count, line, col)?;
                    out.reserve(count);
                    for w in items.windows(n) {
                        out.push(Value::array(w.to_vec()));
                    }
                }
            } else {
                // `chunks` partitions instead of sliding: no element appears twice, and
                // the last group is short when the length does not divide evenly —
                // dropping it would silently lose data, which is worse than a ragged
                // tail the caller can see and handle.
                window_count_guard("chunks", items.len().div_ceil(n), line, col)?;
                for c in items.chunks(n) {
                    out.push(Value::array(c.to_vec()));
                }
            }
            Ok(Value::array(out))
        }
        // `xss.flatten()` — one level: spread each array element, keep scalars. Turns
        // an array of arrays (e.g. dictionary column-groups) into one array.
        "flatten" => {
            if !args.is_empty() {
                return Err(HelixError::new("`flatten` takes no arguments", line, col));
            }
            // Concatenating packed columns needs no boxing at all. The general path below
            // boxes every inner element twice — once when `to_values()` materializes the
            // inner array, again into `out` — before `array_sniff` unpacks the lot again:
            // `[xs].flatten()` on a 20M-element xs cost 797 MB against 186 MB for the
            // array alone. Here the i64/f64 buffers are appended directly, and the result
            // is the same packed column `array_sniff` would have arrived at.
            //
            // Ints and Floats are kept as separate cases rather than one numeric case,
            // because a MIXED nesting (`[[1], [2.0]]`) must still reach `array_sniff`, and
            // `array_sniff` leaves that boxed — packing it to floats here would silently
            // turn an `Int` element into a `Float`.
            use crate::value::ArrayData;
            fn inner(v: &Value) -> Option<&ArrayData> {
                match v {
                    Value::Array(a) => Some(&**a),
                    _ => None,
                }
            }
            let all = |f: fn(&ArrayData) -> bool| {
                !items.is_empty() && items.iter().all(|v| inner(v).is_some_and(f))
            };
            let width = || items.iter().filter_map(inner).map(|a| a.len()).sum();
            if all(|a| matches!(a, ArrayData::Ints(_) | ArrayData::Range { .. })) {
                let mut out: Vec<i64> = Vec::with_capacity(width());
                for a in items.iter().filter_map(inner) {
                    if let Some(xs) = a.to_ints() {
                        out.extend_from_slice(&xs);
                    }
                }
                return Ok(Value::int_array(out));
            }
            if all(|a| matches!(a, ArrayData::Floats(_))) {
                let mut out: Vec<f64> = Vec::with_capacity(width());
                for a in items.iter().filter_map(inner) {
                    if let ArrayData::Floats(xs) = a {
                        out.extend_from_slice(xs);
                    }
                }
                return Ok(Value::float_array(out));
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
                            "`{name}` needs an array of DNA sequences, found {}",
                            crate::value::with_article(other.type_name())
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
                // A sequence with no classifiable base has an unknown GC fraction, and an
                // unknown term makes the mean unknown — the same propagation the arm's
                // first line already applies to a `missing` element (ADR 0001).
                let mut total = 0.0;
                for s in &seqs {
                    match dna_gc(s, name, line, col)? {
                        Some(gc) => total += gc,
                        None => return Ok(Value::Missing),
                    }
                }
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
                    format!("`dot` expects an array, but got {}", crate::value::with_article(o.type_name())),
                    line,
                    col,
                )),
            };
            if items.iter().chain(other.iter()).any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Whether BOTH sides are `Int` is decided before anything is widened, but the
            // conversions and the length check still run first and unchanged, so every
            // error — non-numeric element, mismatched lengths — keeps its exact wording
            // and its exact precedence.
            let int_pair = items.iter().all(|v| matches!(v, Value::Int(_)))
                && other.iter().all(|v| matches!(v, Value::Int(_)));
            let (xs, ys) = (numeric_vec(items, "dot", line, col)?, numeric_vec(&other, "dot", line, col)?);
            if xs.len() != ys.len() {
                return Err(HelixError::new(
                    format!("`dot` needs equal-length arrays, got {} and {}", xs.len(), ys.len()),
                    line,
                    col,
                ));
            }
            // Preserve int-ness, the rule `sum` and `cumsum` already follow: an all-`Int`
            // dot product is an `Int`. Going through `f64` unconditionally made this the
            // only integer reduction that could return a WRONG answer — at n = 1e6,
            // `xs.dot(xs)` was 333332833333127552.0 where `xs.map(it * it).sum()` and
            // `xs.zip(xs).map((a, b) => a * b).sum()` both give the exact 333332833333500000.
            // Off by 372,448, silently, because f64 cannot hold integers past 2^53.
            //
            // `checked` throughout: a single i64*i64 product fits i128, but four of them
            // need not sum inside one, so overflow falls back to the same `f64` expression
            // as before — bit-identical to what this returned for such inputs.
            if int_pair {
                let wide = items.iter().zip(other.iter()).try_fold(0i128, |acc, (a, b)| {
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            (*x as i128).checked_mul(*y as i128).and_then(|p| acc.checked_add(p))
                        }
                        // Unreachable under `int_pair`, and total either way.
                        _ => None,
                    }
                });
                if let Some(n) = wide.and_then(|w| i64::try_from(w).ok()) {
                    return Ok(Value::Int(n));
                }
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
            // TRACKED elements: fold-mul on the tape — same rule as `.sum()`/`.mean()`.
            // (`.max()`/`.min()` stay open: their gradient at a tie needs a subgradient
            // decision the tape has no primitive for yet — see docs/dx-plan.md.)
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(1));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Mul, &acc, v, line, col)?;
                }
                return Ok(acc);
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
            // ONE ORDER, ONE DOMAIN (ADR 0025, question (a), option a1). `argsort` used to
            // have its own policy — propagate `missing`, refuse `Dna` — while `sort` errored
            // on `missing` and accepted `Dna`. Two spellings of one concept disagreeing about
            // both edges is the tax a library author pays by being surprised, and `sort_by`
            // IS `argsort` (see `desugar_sort_by`), so `xs.sort()` and `xs.sort_by(it)` did
            // not even agree with each other. They do now.
            let mut idx: Vec<i64> = (0..items.len() as i64).collect();
            if items.iter().all(|v| v.as_f64().is_some()) {
                idx.sort_by(|&a, &b| numeric_cmp(&items[a as usize], &items[b as usize]));
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                idx.sort_by(|&a, &b| match (&items[a as usize], &items[b as usize]) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                // `ops::compare` has always ordered `Dna`; `argsort` refusing it was the
                // outlier, and DNA ordering is a bio-first flagship's own use case.
                idx.sort_by(|&a, &b| match (&items[a as usize], &items[b as usize]) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().any(|v| matches!(v, Value::Missing)) {
                // `sort`'s wording and hint verbatim — one concept, one message.
                return Err(HelixError::new(
                    "cannot sort: the array has missing values",
                    line,
                    col,
                )
                .hint("drop them explicitly first: `xs.drop_missing().sort()`."));
            } else {
                return Err(HelixError::new(
                    "`argsort` needs an array of all numbers, all strings, or all DNA",
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
            // `lo > hi` is a CALLER ERROR, and it has to be caught here rather than left to
            // `Ord::clamp`/`f64::clamp`, both of which PANIC on it. `[1, 2, 3].clamp(5, 1)`
            // aborted the process with a core dump (exit 134) and `try` could not catch it —
            // an ADR-0024 violation, since user input must never take the host down. The
            // scalar `clamp(x, lo, hi)` builtin has always had this guard; the array method
            // did not, so the same mistake was catchable one way and fatal the other. Same
            // wording and hint as the scalar, so the two agree.
            if lo > hi {
                return Err(HelixError::new(
                    format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                    line,
                    col,
                )
                .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
            }
            // Preserve int-ness when all elements are integral. Selection is written as
            // comparisons rather than `.clamp()` for the second reason that method is
            // unsafe here: it also panics when a bound is NaN, which `lo > hi` cannot
            // detect (every comparison against NaN is false). Comparisons are total — a NaN
            // bound simply matches nothing and the element passes through, exactly as the
            // scalar builtin behaves.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let (loi, hii) = (lo as i64, hi as i64);
                let out: Vec<i64> = items
                    .iter()
                    .map(|v| match v {
                        Value::Int(i) if *i < loi => loi,
                        Value::Int(i) if *i > hii => hii,
                        Value::Int(i) => *i,
                        _ => 0,
                    })
                    .collect();
                Ok(Value::int_array(out))
            } else {
                let xs = numeric_vec(items, "clamp", line, col)?;
                let out: Vec<f64> = xs
                    .iter()
                    .map(|x| if *x < lo { lo } else if *x > hi { hi } else { *x })
                    .collect();
                Ok(Value::float_array(out))
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

/// The hash key of a text value, BORROWED from the array so probing the table mints
/// no `String`. The DNA-ness is part of the key because `values_equal` does **not**
/// equate `dna("AT")` with `"AT"`: it has a `(Str, Str)` arm and a `(Dna, Dna)` arm,
/// and the cross pair falls to `_ => false` — which is what `contains`/`index_of`
/// report. Keying on the bytes alone silently merged them, so `[dna("AT"), "AT"]`
/// had `unique().length() == 1` while `index_of("AT") == 1` said they were distinct.
fn text_key(v: &Value) -> (bool, &str) {
    match v {
        Value::Dna(s) => (true, s.as_str()),
        Value::Str(s) => (false, s.as_str()),
        _ => unreachable!("callers guard with an `all(Str | Dna)` kind check"),
    }
}

/// The hash key of an `Int`-or-`missing` array. `values_equal` treats all missings as
/// one identity and never equates a missing with an integer (ADR 0001), and integers
/// compare as exact `i64` — so these two variants reproduce its classes EXACTLY.
/// An array holding any `Float` or `Rational` may not use this key: `values_equal`
/// collapses `1 == 1.0` across types, and above 2^53 that collapse is not even
/// transitive (`9007199254740993` and `…92` are both equal to `9007199254740992.0`
/// but not to each other), so no hash key can reproduce it. Those keep the scan.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum IntKey {
    N(i64),
    Missing,
}

fn int_key(v: &Value) -> IntKey {
    match v {
        Value::Int(n) => IntKey::N(*n),
        Value::Missing => IntKey::Missing,
        _ => unreachable!("callers guard with an `all(Int | Missing)` kind check"),
    }
}

/// One pass, ONE hash probe per element, first-seen order preserved. `key` must
/// reproduce `values_equal`'s equivalence classes exactly over `items` — every
/// caller establishes that with an `all(...)` kind check before picking a key.
///
/// No `with_capacity(items.len())`: distinct keys are usually FAR fewer than elements
/// (a k-mer spectrum is the design centre), and reserving one bucket per element built
/// a 5M-bucket table — whose control bytes alone are memset — to hold 10k entries.
/// Growth is amortized O(1), and dropping the reserve measured faster in BOTH regimes:
/// 0.33s -> 0.068s at 10k-distinct/5M, and 1.83s -> 1.75s (-200 MB) even when every
/// element is distinct, which is the only case the reserve could have helped.
/// The hash key of a `Float`-or-`missing` array. Two wrinkles that the `Int` key does not
/// have, and both are why floats were left out of the first pass:
///
/// * `-0.0 == 0.0` is TRUE, but their bit patterns differ — so zero is canonicalized and
///   the first of the pair seen stays the representative, exactly as the scan would leave it.
/// * **NaN is not equal to itself**, so a NaN belongs to no equivalence class at all. It
///   gets `None`: no key, no table entry, a fresh bucket every time — which is precisely
///   what the `values_equal` scan produced, since `NaN == NaN` is false there too.
///
/// As with `IntKey`, an array holding BOTH `Int` and `Float` may not use this: `values_equal`
/// collapses `1 == 1.0`, and above 2^53 that collapse is not transitive.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum FloatKey {
    Bits(u64),
    Missing,
}

fn float_key(v: &Value) -> Option<FloatKey> {
    match v {
        Value::Float(f) if f.is_nan() => None,
        // `+0.0` and `-0.0` compare equal, so they must hash equal.
        Value::Float(f) => Some(FloatKey::Bits(if *f == 0.0 { 0.0f64 } else { *f }.to_bits())),
        Value::Missing => Some(FloatKey::Missing),
        _ => unreachable!("callers guard with an `all(Float | Missing)` kind check"),
    }
}

/// One pass, ONE hash probe per element, first-seen order preserved. `key` must reproduce
/// `values_equal`'s equivalence classes EXACTLY over `items` — every caller establishes that
/// with an `all(...)` kind check before picking a key. A `None` key means the value is equal
/// to NOTHING, not even another copy of itself (only NaN), so it takes a fresh bucket and
/// never enters the table.
///
/// No `with_capacity(items.len())`: distinct keys are usually FAR fewer than elements
/// (a k-mer spectrum is the design centre), and reserving one bucket per element built
/// a 5M-bucket table — whose control bytes alone are memset — to hold 10k entries.
/// Growth is amortized O(1), and dropping the reserve measured faster in BOTH regimes:
/// 0.33s -> 0.068s at 10k-distinct/5M, and 1.83s -> 1.75s (-200 MB) even when every
/// element is distinct, which is the only case the reserve could have helped.
fn tally<'a, K: Eq + std::hash::Hash>(
    items: &'a [Value],
    key: impl Fn(&'a Value) -> Option<K>,
) -> Vec<(Value, i64)> {
    use std::collections::hash_map::Entry;
    let mut idx: std::collections::HashMap<K, usize> = std::collections::HashMap::new();
    let mut counts: Vec<(Value, i64)> = Vec::new();
    for v in items.iter() {
        let next = counts.len();
        let Some(k) = key(v) else {
            counts.push((v.clone(), 1));
            continue;
        };
        match idx.entry(k) {
            Entry::Occupied(e) => counts[*e.get()].1 += 1,
            Entry::Vacant(e) => {
                e.insert(next);
                counts.push((v.clone(), 1));
            }
        }
    }
    counts
}

/// Value-count histogram, sorted by count desc then value asc — the shared core of
/// `top`/`frequencies`. Text arrays (k-mer spectra) and `Int`/`missing` arrays take a
/// ~O(n) hash path; everything else falls back to the value-equality scan, which
/// honors cross-type numeric equality (`1 == 1.0`) that no hash key can express.
/// Insertion order is preserved before the sort, matching the old `top`.
///
/// The `Int` path is not a micro-optimization: the scan is O(n × distinct), so a 5M
/// histogram over 10k distinct integers ran 2.5e10 `values_equal` calls — 41.7s, versus
/// 0.06s for the SAME histogram spelled with string keys. `unique` had had an all-Int
/// hash path for exactly this reason; `frequencies` never got one.
fn value_histogram(items: &[Value]) -> Vec<(Value, i64)> {
    let mut counts = if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
        tally(items, |v| Some(text_key(v)))
    } else if items.iter().all(|v| matches!(v, Value::Int(_) | Value::Missing)) {
        tally(items, |v| Some(int_key(v)))
    } else if items.iter().all(|v| matches!(v, Value::Float(_) | Value::Missing)) {
        // Floats were left out of the first pass and it cost 220x on the element type
        // scientific data actually uses: `(0..60000).map(to_float(it)).unique()` took 3.2s,
        // while stringifying every float and hashing the TEXT took 0.04s — the same
        // O(n × distinct) scan the integer path was rescued from.
        tally(items, float_key)
    } else {
        let mut counts: Vec<(Value, i64)> = Vec::new();
        for v in items.iter() {
            if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                e.1 += 1;
            } else {
                counts.push((v.clone(), 1));
            }
        }
        counts
    };
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
        // `chars()` — the string as an array of one-character strings, so the whole array
        // vocabulary (`map`/`filter`/`reduce`/`enumerate`) applies to text. Without it the
        // linear-time spelling was `s.replace("", "\t").split("\t")`, which is both
        // undiscoverable and WRONG — it yields `["", "a", "b", "c", ""]`, with an empty
        // string at each end. The obvious `s[i]` walk is quadratic (each index counts
        // scalars from the start), so the only correct spelling was also the hidden one.
        //
        // Unicode SCALARS, matching `count`/`length`/`reverse`/`take`/`drop`, which all
        // measure the same unit. Not bytes, and not grapheme clusters (which need a table
        // and would disagree with every other method here).
        "chars" => {
            arity(0)?;
            Ok(Value::array(
                s.chars().map(|c| Value::Str(Rc::new(c.to_string()))).collect::<Vec<_>>(),
            ))
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
        // Where a needle starts, as a CHARACTER index — the unit every other String
        // method counts in (`take`, `drop`, `chars`, `s[a:b]`), not the byte offset
        // the underlying search returns. Answering in bytes would be a silent trap on
        // any non-ASCII input, since the index is meant to be fed straight back to
        // `take`/`drop`. `missing` when absent, exactly like an array's `index_of`.
        // An empty needle answers 0, matching its neighbour `contains("")` — the two
        // ask the same question, so they agree.
        "index_of" => {
            arity(1)?;
            let needle = str_arg(args, 0, name, line, col)?;
            Ok(match s.find(needle) {
                Some(byte) => Value::Int(s[..byte].chars().count() as i64),
                None => Value::Missing,
            })
        }
        // The LAST occurrence — a CHARACTER index exactly like `index_of` (a byte
        // index would silently mislocate anything past a multi-byte character).
        "last_index_of" => {
            arity(1)?;
            let needle = str_arg(args, 0, name, line, col)?;
            Ok(match s.rfind(needle) {
                Some(byte) => Value::Int(s[..byte].chars().count() as i64),
                None => Value::Missing,
            })
        }
        // `split` at the FIRST separator only, keeping the rest of the tail intact:
        // `(before, after)`, or `missing` when the separator does not occur. The
        // spelling this replaces was `let eq = part.split("="), k = eq[0], v = if
        // eq.count() <= 1 then "" else part.drop(k.count() + 1)` — split everything,
        // discard the rest, then recover the tail by arithmetic on the first part's
        // length, which is easy to get wrong by one. An empty separator is refused
        // exactly as `split` refuses it: same argument role, same rule.
        "split_once" => {
            arity(1)?;
            let sep = str_arg(args, 0, name, line, col)?;
            if sep.is_empty() {
                return Err(HelixError::new("`split_once` separator cannot be empty", line, col)
                    .hint("split on a non-empty string, e.g. `s.split_once(\"=\")`."));
            }
            Ok(match s.split_once(sep) {
                Some((a, b)) => Value::Tuple(Rc::new(vec![
                    Value::Str(Rc::new(a.to_string())),
                    Value::Str(Rc::new(b.to_string())),
                ])),
                None => Value::Missing,
            })
        }
        // The sibling of `Array.concat`. Interpolation ("{a}{b}") remains the everyday
        // way to build a string; this exists so that a verb which joins two sequences
        // means the same thing on both sequence types, which is the whole of the
        // surprise the review reported.
        "concat" => {
            arity(1)?;
            let other = str_arg(args, 0, name, line, col)?;
            let mut out = String::with_capacity(s.len() + other.len());
            out.push_str(s);
            out.push_str(other);
            Ok(Value::Str(Rc::new(out)))
        }
        "replace" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replace(from, to))))
        }
        // `replace` swaps EVERY occurrence; this swaps exactly the first — the
        // pair every string library ends up needing both halves of.
        "replace_first" => {
            arity(2)?;
            let from = str_arg(args, 0, name, line, col)?;
            let to = str_arg(args, 1, name, line, col)?;
            Ok(Value::Str(Rc::new(s.replacen(from, to, 1))))
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
            // GC fraction over *classifiable* bases — see `simd::gc_counts` for the
            // policy: `S` ("G or C") is GC, `W` ("A or T") is not, and every code that
            // could be either (`N`, `R Y K M B D H V`) is excluded from numerator AND
            // denominator, so `gc_content("GCN") == 1.0`, not 2/3, and `"GCS"` reads
            // 1.0 rather than LOWER than the same sequence without the S. A sequence
            // with no classifiable base has an unknown fraction: `missing` (ADR 0001),
            // because 0.0 here is indistinguishable from a genuinely AT-only answer.
            match dna_gc(s, "gc_content", line, col)? {
                Some(gc) => Ok(Value::Float(gc)),
                None => Ok(Value::Missing),
            }
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
                        format!("`find` needs a string or DNA pattern, but got {}", crate::value::with_article(v.type_name())),
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
                        format!("`find_all` needs a string or DNA pattern, but got {}", crate::value::with_article(v.type_name())),
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
            // AT fraction = 1 − GC fraction, over the same classifiable-base policy —
            // which is what makes `dna("S").at_content()` answer 0.0 (S is never A or
            // T) instead of the old 1.0, and keeps `gc_content + at_content == 1.0`
            // whenever either is a number at all.
            match dna_gc(s, "at_content", line, col)? {
                Some(gc) => Ok(Value::Float(1.0 - gc)),
                None => Ok(Value::Missing),
            }
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
                        format!("`hamming` needs a DNA or string sequence, but got {}", crate::value::with_article(v.type_name())),
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
                km.push(b"ACGT"[b as usize] as char);
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
pub(crate) fn ufcs_fallback_applies(recv: &Value, name: &str) -> bool {
    match recv {
        Value::PyObject(_)
        | Value::DataFrame(_)
        | Value::GroupBy(_) => false,
        Value::Node(_) => !crate::autodiff::is_tape_method(name),
        other => !crate::registry::type_owns_method(other.type_name(), name),
    }
}


/// Methods on a [`Value::Headers`]. Every name lookup is CASE-INSENSITIVE — that is the
/// type's whole reason to exist — and iteration answers wire order with names as they
/// arrived. `get` returns the FIRST match (the one a proxy would act on), `get_all`
/// every match in order, because a repeated name (`Set-Cookie`) is data, not a
/// collision. Header counts are capped upstream, so the linear scans here are bounded.
fn headers_method(
    pairs: &Rc<Vec<(String, String)>>,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let want_name = |what: &str| -> Result<&str, HelixError> {
        match args {
            [Value::Str(s)] => Ok(s.as_str()),
            _ => Err(HelixError::new(
                format!("`{name}` takes {what}"),
                line,
                col,
            )
            .hint("e.g. `r.headers.get(\"Content-Type\")` — the lookup ignores case.")),
        }
    };
    match name {
        "get" => {
            let k = want_name("a header name")?;
            Ok(pairs
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| Value::Str(Rc::new(v.clone())))
                .unwrap_or(Value::Missing))
        }
        "get_all" => {
            let k = want_name("a header name")?;
            Ok(Value::array(
                pairs
                    .iter()
                    .filter(|(n, _)| n.eq_ignore_ascii_case(k))
                    .map(|(_, v)| Value::Str(Rc::new(v.clone())))
                    .collect(),
            ))
        }
        "has" | "contains" => {
            let k = want_name("a header name")?;
            Ok(Value::Bool(pairs.iter().any(|(n, _)| n.eq_ignore_ascii_case(k))))
        }
        "keys" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs.iter().map(|(n, _)| Value::Str(Rc::new(n.clone()))).collect(),
            ))
        }
        "values" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs.iter().map(|(_, v)| Value::Str(Rc::new(v.clone()))).collect(),
            ))
        }
        "items" => {
            no_args(name, args, line, col)?;
            Ok(Value::array(
                pairs
                    .iter()
                    .map(|(n, v)| {
                        Value::Tuple(Rc::new(vec![
                            Value::Str(Rc::new(n.clone())),
                            Value::Str(Rc::new(v.clone())),
                        ]))
                    })
                    .collect(),
            ))
        }
        "count" | "length" => {
            no_args(name, args, line, col)?;
            Ok(Value::Int(pairs.len() as i64))
        }
        // The escape hatch to plain-Dict land: LOWERCASED keys (the canonical form),
        // FIRST occurrence wins, matching what `get` answers. Order and repeats are
        // exactly what this conversion gives up, and taking it is the caller saying so.
        "to_dict" => {
            no_args(name, args, line, col)?;
            let mut map = std::collections::BTreeMap::new();
            for (n, v) in pairs.iter() {
                map.entry(crate::value::DictKey::Str(Rc::new(n.to_ascii_lowercase())))
                    .or_insert_with(|| Value::Str(Rc::new(v.clone())));
            }
            Ok(Value::Dict(Rc::new(map)))
        }
        _ => Err(unknown_method(
            "Headers",
            name,
            &crate::registry::methods_of(crate::registry::HEADERS_METHODS),
            line,
            col,
        )),
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
