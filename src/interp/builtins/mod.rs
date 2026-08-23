//! The built-in function dispatch (`print`, math, `range`, `dna`, `tensor`,
//! `read_csv`/`read_parquet`/`read_fasta`, `write_parquet`, …) — an `impl Interp`
//! method, split out from the core evaluator. The numeric/shape helper free
//! functions it uses (`broadcast_unary`, `apply_float_fn`, `int_range`, …) stay in
//! the parent module and are reached via `use super::*`.

use super::*;

/// How many assertions this process has executed, counted at the one dispatch point the
/// tree-walker and the VM share. `helix test` reads it to fail a test file that ran to
/// completion without asserting anything — a file whose checks all sit inside `test_*`
/// functions nobody calls used to report `ok`, which is the worst possible answer from a
/// test runner. Not reset by `helix run`; `cli_test` resets it per file.
pub static ASSERTIONS_RUN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl super::Interp {
    pub(crate) fn call_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        // Capability gate (ADR 0021): authority-bearing builtins (fs/net) consult the
        // process authority first. A no-op under the default `Off` mode and for `pure`
        // builtins; logs (audit) or denies (enforce) an ungranted access otherwise.
        crate::capability::gate(name, &args, line, col)?;
        // Counted before the arm runs, so a FAILING assertion counts too: the file fails
        // on the raise, and "asserted nothing" must not also be reported about it.
        if matches!(name, "assert" | "assert_eq" | "assert_close") {
            ASSERTIONS_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // The dispatch chain: each topical module answers its own names and
        // hands the arguments back otherwise. Order is cold-to-hot agnostic —
        // every module's guard is one `matches!` over its literal names.
        let args = args;
        let args = match output::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match corefns::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match mathfns::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match stats::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match autodiff_fns::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match tensors::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match frames::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match encoding::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match io::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match net::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let args = match bio::call(name, args, line, col) {
            Called::Done(r) => return r,
            Called::Not(a) => a,
        };
        let _ = args;
        let err =
            HelixError::new(format!("`{}` is not a known function", name), line, col);
        Err(match crate::suggest::hint(name, crate::suggest::Site::Function, &[]) {
            Some(h) => err.hint(h),
            None => err,
        })
    }
}

/// One topical module's answer: it either OWNED the name (Done) or hands the
/// arguments back untouched (Not) for the next module in the chain.
pub(super) enum Called {
    Done(Result<Value, HelixError>),
    Not(Vec<Value>),
}

mod output;
mod corefns;
mod mathfns;
mod stats;
mod autodiff_fns;
mod tensors;
mod frames;
mod encoding;
mod io;
mod net;
mod bio;


/// Convert a Helix array (one column of a `dataframe({…})` call) into a
/// backend-agnostic [`ColData`]. The column type is inferred from the first
/// non-`missing` element; `missing` becomes a null (selecting the nullable
/// variant). Ints + Floats in one column promote to Float; a column may not mix
/// otherwise-incompatible types.
fn array_to_coldata(
    name: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<crate::backend::ColData, HelixError> {
    use crate::backend::ColData;
    let arr = match v {
        Value::Array(a) => a,
        other => {
            return Err(HelixError::new(
                format!("column `{}` must be an array, but got {}", name, crate::value::with_article(other.type_name())),
                line,
                col,
            )
            .hint("each DataFrame column is an array, e.g. `dataframe({age: [30, 41]})`."))
        }
    };
    let vals = arr.to_values();
    let mixed = |got: &Value| {
        HelixError::new(
            format!("column `{}` mixes types (found {})", name, crate::value::with_article(got.type_name())),
            line,
            col,
        )
        .hint("each DataFrame column must be all one type.")
    };
    let has_missing = vals.iter().any(|x| matches!(x, Value::Missing));
    let kind = match vals.iter().find(|x| !matches!(x, Value::Missing)) {
        Some(k) => k,
        None => {
            return Err(HelixError::new(
                format!("cannot infer the type of column `{}` (empty or all `missing`)", name),
                line,
                col,
            ))
        }
    };
    match kind {
        Value::Int(_) | Value::Float(_) => {
            let any_float = vals.iter().any(|x| matches!(x, Value::Float(_)));
            if any_float {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i as f64)),
                        Value::Float(f) => out.push(Some(*f)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Float(out))
            } else if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::IntOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(*i),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Int(out))
            }
        }
        Value::Str(_) | Value::Dna(_) => {
            let pull = |x: &Value| match x {
                Value::Str(s) => Some((**s).clone()),
                Value::Dna(s) => Some((**s).clone()),
                _ => None,
            };
            if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Missing => out.push(None),
                        _ => match pull(x) {
                            Some(s) => out.push(Some(s)),
                            None => return Err(mixed(x)),
                        },
                    }
                }
                Ok(ColData::StrOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match pull(x) {
                        Some(s) => out.push(s),
                        None => return Err(mixed(x)),
                    }
                }
                Ok(ColData::Str(out))
            }
        }
        Value::Bool(_) => {
            if has_missing {
                return Err(HelixError::new(
                    format!("boolean column `{}` cannot contain `missing`", name),
                    line,
                    col,
                ));
            }
            let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
            for x in vals.iter() {
                match x {
                    Value::Bool(b) => out.push(*b),
                    o => return Err(mixed(o)),
                }
            }
            Ok(ColData::Bool(out))
        }
        other => Err(HelixError::new(
            format!("column `{}` has an unsupported element type ({})", name, other.type_name()),
            line,
            col,
        )
        .hint("DataFrame columns must be numbers, strings, DNA, or booleans.")),
    }
}

/// Extract a slice of numeric columns from an array-of-arrays argument (the predictor
/// matrix of `multiple_regression`). Returns `Ok(None)` if any element anywhere is
/// `missing`; errors if the outer value is not an array, or any inner value is not a
/// numeric array.
fn num_arrays(
    who: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Option<Vec<Vec<f64>>>, HelixError> {
    let outer = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of predictor arrays", other, line, col)),
    };
    let mut cols = Vec::with_capacity(outer.len());
    for el in outer.to_values().iter() {
        match num_array(who, el, line, col)? {
            Some(c) => cols.push(c),
            None => return Ok(None),
        }
    }
    Ok(Some(cols))
}

/// Greatest common divisor of two i64 (Euclid). Sign-agnostic; gcd(n,0)=|n|.
/// Computed in i128 so `abs(i64::MIN)` doesn't overflow.
fn gcd_i64(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = ((a as i128).unsigned_abs(), (b as i128).unsigned_abs());
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a as i64
}

/// All primes below `n` (empty for `n <= 2`) via a byte-array Sieve of Eratosthenes —
/// strikes start at `p*p` (no usize overflow: `p < n <= 1e8` keeps `p*p < 1e16`).
/// O(n log log n) time, O(n) bytes for the composite flags.
fn sieve_primes(n: i64) -> Vec<i64> {
    if n <= 2 {
        return Vec::new();
    }
    let n = n as usize;
    let mut composite = vec![false; n];
    let mut out = Vec::new();
    for p in 2..n {
        if !composite[p] {
            out.push(p as i64);
            let mut m = p * p;
            while m < n {
                composite[m] = true;
                m += p;
            }
        }
    }
    out
}

/// Integer square root: the largest `x >= 0` with `x*x <= n`. Caller guarantees `n >= 0`.
/// An f64 seed corrected in i128 — exact even near `i64::MAX`, where `sqrt() as i64` can be
/// off by one and the verification `x*x` would overflow i64.
fn isqrt_i64(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    let n = n as i128;
    let mut x = (n as f64).sqrt() as i128;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x as i64
}

/// Parse an even-length hex string (`0-9a-fA-F`) into its bytes; `None` on odd length or a
/// non-hex digit. Shared by `hex_decode` and the AES key parser.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let hb = s.as_bytes();
    if !hb.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hb.len() / 2);
    for pair in hb.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Parse a 32-byte (64-hex-char) AES-256 key, with a clear error otherwise.
fn aes_key_bytes(hex: &str, line: usize, col: usize) -> Result<Vec<u8>, HelixError> {
    match hex_to_bytes(hex) {
        Some(b) if b.len() == 32 => Ok(b),
        _ => Err(HelixError::new(
            "an AES key must be 64 hex characters (32 bytes) — generate one with `aes_keygen()`",
            line,
            col,
        )),
    }
}

/// Parse exactly 32 bytes from a 64-hex string into a fixed array (Ed25519 keys), with a
/// `what`-labelled error otherwise.
fn hex_to_array32(hex: &str, what: &str, line: usize, col: usize) -> Result<[u8; 32], HelixError> {
    match hex_to_bytes(hex).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
        Some(a) => Ok(a),
        None => Err(HelixError::new(format!("{what} must be 64 hex characters (32 bytes)"), line, col)),
    }
}

/// Extract `(method, url, body, headers)` from an `http_request`/`http_stream` request record
/// (`{method, url, body?, headers?}`). `headers` may be a Record (identifier names), a Dict,
/// or an array of `[name, value]` pairs (the inline-friendly form for dash-named headers,
/// since Helix has no dict literal). Shared by both client verbs.
/// The parsed parts of an http request record: `(method, url, body, headers)`.
type HttpReqParts = (String, String, String, Vec<(String, String)>);

#[cfg_attr(not(feature = "http"), allow(dead_code))]
/// Refuse an unrecognized field in a request record, naming it and listing the
/// ones that are read. A typo'd `timeout_ms` (for `total_ms`) silently left a
/// request with NO total deadline, and `cookies:` (for `jar:`) a session with
/// no cookies and no error anywhere — the field report's §1.4. The rule is the
/// one `helix.toml` already applies: an unknown key is a hard error, because a
/// field that silently does nothing is a bug that ships. Non-record shapes fall
/// through — the field readers own those errors.
fn validate_request_fields(
    req: &Value,
    allowed: &[&str],
    who: &str,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let Value::Record(fields) = req else { return Ok(()) };
    for (k, _) in fields.iter() {
        let k = k.as_str();
        if !allowed.contains(&k) {
            let hint = match k {
                "cookies" => "the cookie-jar field is `jar:` (a jar from `cookie_jar()`).".to_string(),
                "timeout_ms" if who == "http_request" => {
                    "the deadline fields are `total_ms` (whole request), `connect_ms`, and `read_ms`."
                        .to_string()
                }
                _ => format!("fields read: {}.", allowed.join(", ")),
            };
            return Err(HelixError::new(
                format!("`{who}` does not read a field named `{k}`"),
                line,
                col,
            )
            .hint(hint));
        }
    }
    Ok(())
}

fn http_request_fields(req: &Value, line: usize, col: usize) -> Result<HttpReqParts, HelixError> {
    let Value::Record(fields) = req else {
        return Err(type_err("http_request", "a `{ method, url, … }` record", req, line, col));
    };
    let field = |k: &str| fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v);
    let str_field = |v: &Value, what: &str| -> Result<String, HelixError> {
        match v {
            Value::Str(s) => Ok((**s).clone()),
            other => Err(type_err("http_request", what, other, line, col)),
        }
    };
    let method = match field("method") {
        Some(v) => str_field(v, "a string `method`")?.to_uppercase(),
        None => {
            return Err(HelixError::new("the request record needs a `method` field", line, col)
                .hint("e.g. `{method: \"PUT\", url: u, body: b}`"));
        }
    };
    let url = match field("url") {
        Some(v) => str_field(v, "a string `url`")?,
        None => return Err(HelixError::new("the request record needs a `url` field", line, col)),
    };
    let body = match field("body") {
        Some(v) => str_field(v, "a string `body`")?,
        None => String::new(),
    };
    let hval = |v: &Value| match v {
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    };
    let mut hdrs: Vec<(String, String)> = Vec::new();
    match field("headers") {
        Some(Value::Record(hf)) => {
            for (k, v) in hf.iter() {
                hdrs.push((k.as_str().to_string(), hval(v)));
            }
        }
        Some(Value::Dict(map)) => {
            for (k, v) in map.iter() {
                if let crate::value::DictKey::Str(s) = k {
                    hdrs.push(((**s).clone(), hval(v)));
                }
            }
        }
        // A Headers value round-trips: forwarding a response's headers into the
        // next request is the proxy shape, and it should not need a conversion.
        Some(Value::Headers(pairs)) => {
            for (k, v) in pairs.iter() {
                hdrs.push((k.clone(), v.clone()));
            }
        }
        Some(Value::Array(items)) => {
            for it in items.to_values().iter() {
                let two: Vec<Value> = match it {
                    Value::Array(a) => a.to_values().to_vec(),
                    Value::Tuple(t) => t.iter().cloned().collect(),
                    _ => continue,
                };
                if let [Value::Str(k), v] = two.as_slice() {
                    hdrs.push(((**k).clone(), hval(v)));
                }
            }
        }
        _ => {}
    }
    // Every request header, whatever shape it arrived in, before it can reach the wire.
    for (k, val) in &hdrs {
        crate::value::validate_header(k, val).map_err(|m| HelixError::new(m, line, col))?;
    }
    Ok((method, url, body, hdrs))
}

/// Read the optional `timeout_ms` field from an `http_stream` request record — a positive
/// integer per-chunk read deadline in milliseconds. Absent → `None` (no read timeout). A
/// non-integer or non-positive value is a clean error rather than a silently-ignored field.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
/// The optional limit fields of an `http_request` record (ADR 0031 §3): `total_ms`,
/// `connect_ms`, `read_ms` (positive milliseconds) and `max_body` (positive bytes).
/// Absent fields keep the defaults the client has always used; a present field with a
/// non-positive or non-integer value is a clean error naming the field, not a
/// silently-ignored one.
/// The optional `jar:` field of a request record — a `cookie_jar()` handle, or `None`.
/// A `jar` that is not a cookie jar is ignored rather than erroring: the field is
/// optional, and a wrong value there is the caller's to notice, not a request-breaker.
#[cfg(feature = "http")]
fn http_jar(req: &Value) -> Option<Rc<crate::serve::NetHandle>> {
    let Value::Record(fields) = req else { return None };
    match fields.iter().find(|(s, _)| s.as_str() == "jar").map(|(_, v)| v) {
        Some(Value::Net(h)) if matches!(&**h, crate::serve::NetHandle::CookieJar(_)) => {
            Some(h.clone())
        }
        _ => None,
    }
}

#[cfg(feature = "http")]
fn http_limits(req: &Value, line: usize, col: usize) -> Result<crate::http::Limits, HelixError> {
    let Value::Record(fields) = req else { return Ok(Default::default()) };
    let pos = |name: &str| -> Result<Option<u64>, HelixError> {
        match fields.iter().find(|(s, _)| s.as_str() == name).map(|(_, v)| v) {
            None | Some(Value::Missing) => Ok(None),
            Some(Value::Int(n)) if *n > 0 => Ok(Some(*n as u64)),
            Some(_) => Err(HelixError::new(
                format!("`{name}` must be a positive integer"),
                line,
                col,
            )
            .hint("timeouts are milliseconds, `max_body` is bytes — e.g. `total_ms: 5000`.")),
        }
    };
    Ok(crate::http::Limits {
        total_ms: pos("total_ms")?,
        connect_ms: pos("connect_ms")?,
        read_ms: pos("read_ms")?,
        max_body: pos("max_body")?.map(|n| n as usize),
    })
}

fn http_timeout_ms(req: &Value, line: usize, col: usize) -> Result<Option<u64>, HelixError> {
    let Value::Record(fields) = req else { return Ok(None) };
    match fields.iter().find(|(s, _)| s.as_str() == "timeout_ms").map(|(_, v)| v) {
        None | Some(Value::Missing) => Ok(None),
        Some(Value::Int(n)) if *n > 0 => Ok(Some(*n as u64)),
        Some(_) => Err(HelixError::new(
            "`timeout_ms` must be a positive integer (milliseconds)",
            line,
            col,
        )
        .hint("e.g. `http_stream({method: \"POST\", url: u, body: b, timeout_ms: 5000})`.")),
    }
}

/// Read a whole file to a `String`, capping at `MAX_STRING_LEN` so a huge file is a
/// clean error rather than an allocator abort. Shared by `read_text` and `read_json`.
fn read_text_file(path: &str, line: usize, col: usize) -> Result<String, HelixError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        HelixError::new(format!("could not read `{path}`: {e}"), line, col)
            .hint("check the path exists and is readable.")
    })?;
    if meta.len() as usize > crate::interp::MAX_STRING_LEN {
        return Err(HelixError::new(
            format!("`{path}` is larger than the {}-byte text limit", crate::interp::MAX_STRING_LEN),
            line,
            col,
        )
        .hint("read very large tabular files with read_csv/read_parquet instead."));
    }
    std::fs::read_to_string(path)
        .map_err(|e| HelixError::new(format!("could not read `{path}`: {e}"), line, col))
}

/// Extract two equal-length, non-empty label arrays as raw `Value`s for the
/// classification metrics. Labels are compared by value equality — never coerced to
/// f64 — so integer (0/1), boolean, and string class labels all work uniformly.
fn label_pair(
    who: &str,
    yt: &Value,
    yp: &Value,
    line: usize,
    col: usize,
) -> Result<(Vec<Value>, Vec<Value>), HelixError> {
    let take = |v: &Value| -> Result<Vec<Value>, HelixError> {
        match v {
            Value::Array(items) => Ok(items.to_values().into_owned()),
            other => Err(type_err(who, "an array of labels", other, line, col)),
        }
    };
    let (a, b) = (take(yt)?, take(yp)?);
    if a.len() != b.len() {
        return Err(HelixError::new(
            format!("`{who}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
            line,
            col,
        ));
    }
    if a.is_empty() {
        return Err(HelixError::new(format!("cannot compute `{who}` of empty arrays"), line, col));
    }
    Ok((a, b))
}

/// Tally (tp, fp, fn, tn) of `pred` against `truth` for one positive class `pos`.
fn binary_counts(truth: &[Value], pred: &[Value], pos: &Value) -> (i64, i64, i64, i64) {
    let (mut tp, mut fp, mut fa_neg, mut tn) = (0i64, 0i64, 0i64, 0i64);
    for (yt, yp) in truth.iter().zip(pred) {
        match (values_equal(yt, pos), values_equal(yp, pos)) {
            (true, true) => tp += 1,
            (false, true) => fp += 1,
            (true, false) => fa_neg += 1,
            (false, false) => tn += 1,
        }
    }
    (tp, fp, fa_neg, tn)
}

/// Extract a numeric `Vec<f64>` from an array argument. Returns `Ok(None)` when any
/// element is `missing` *or* a `NaN` float (so the caller can propagate `missing`,
/// per ADR-0001 — a `NaN` would otherwise silently corrupt the bivariate result),
/// and errors if the value is not an array or holds a non-numeric element.
fn num_array(who: &str, v: &Value, line: usize, col: usize) -> Result<Option<Vec<f64>>, HelixError> {
    let items = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of numbers", other, line, col)),
    };
    let mut out = Vec::with_capacity(items.len());
    for el in items.to_values().iter() {
        match el {
            Value::Missing => return Ok(None),
            Value::Float(f) if f.is_nan() => return Ok(None),
            _ => match el.as_f64() {
                Some(x) => out.push(x),
                None => return Err(type_err(who, "an array of numbers", el, line, col)),
            },
        }
    }
    Ok(Some(out))
}

/// Parse a string to a float for `to_float(s)` and `String.to_float()`. Leading/trailing
/// whitespace is ignored; a non-numeric string is a clear error (not a silent NaN).
pub(crate) fn parse_str_float(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<f64>().map(Value::Float).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as a number"), line, col)
            .hint("`to_float` expects a numeric string like \"3.14\" or \"-2\".")
    })
}

/// Parse a string to an integer for `to_int(s)` and `String.to_int()`. Strict: a decimal
/// string like "3.5" is rejected (use `to_float`), so an integer field never rounds silently.
pub(crate) fn parse_str_int(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<i64>().map(Value::Int).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as an integer"), line, col)
            .hint("`to_int` expects an integer string like \"42\" or \"-7\"; for decimals use `to_float`.")
    })
}

/// An IEEE float predicate (`is_nan`/`is_finite`/`is_infinite`): `Float` → `Bool` via `f`;
/// an `Int`/`Rational` is exact so it yields `on_exact` (always finite, never NaN/inf);
/// `missing` propagates; an array maps elementwise to a `Bool` array; a tensor (f64-only,
/// so no `Bool` element type) yields a `1.0`/`0.0` mask tensor usable in arithmetic.
fn float_predicate(
    v: &Value,
    name: &str,
    f: fn(f64) -> bool,
    on_exact: bool,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match v {
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> = items
                .to_values()
                .iter()
                .map(|e| float_predicate(e, name, f, on_exact, line, col))
                .collect();
            Ok(Value::array(out?))
        }
        Value::Tensor(t) => {
            let data: Vec<f64> = t.iter().map(|&x| if f(x) { 1.0 } else { 0.0 }).collect();
            let out = ndarray::ArrayD::from_shape_vec(t.raw_dim(), data)
                .expect("same length as source tensor");
            Ok(Value::Tensor(Rc::new(out)))
        }
        Value::Missing => Ok(Value::Missing),
        Value::Float(x) => Ok(Value::Bool(f(*x))),
        Value::Int(_) | Value::Rational(_) => Ok(Value::Bool(on_exact)),
        other => Err(type_err(name, "a number or array of numbers", other, line, col)),
    }
}

