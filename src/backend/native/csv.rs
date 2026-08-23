//! CSV in and out (ADR 0034 §8; parallel read = ADR 0033 Stage 3): header row,
//! dtype inference over the first 100 records (Int ⊂ Float ⊂ Str; `true`/`false`
//! make Bool; empty is missing; no date parsing), RFC 4180 quoting both
//! directions.
//!
//! The reader is two passes. Pass 1 is a sequential quote-parity scan that finds
//! TRUE record boundaries (a quoted field may contain newlines, so splitting on
//! `\n` alone would shear records); it is a branch-light byte walk and fast.
//! Pass 2 parses fields and builds typed column segments **in parallel** over
//! row chunks (rayon). Determinism holds by construction: chunks map to fixed
//! row ranges, results splice in chunk order, and when several chunks error the
//! EARLIEST row's error wins — thread count can never change output or error.
//! Writing stays on the `csv` crate.

use std::rc::Rc;

use rayon::prelude::*;

use crate::error::HelixError;

use super::columns::Col;
use super::NativeFrame;

/// How many records the dtype inference window reads before committing.
const INFER_ROWS: usize = 100;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    Unknown,
    Int,
    Float,
    Bool,
    Str,
}

impl Ty {
    /// The join in the Int ⊂ Float ⊂ Str lattice (Bool only merges with itself).
    fn join(self, other: Ty) -> Ty {
        use Ty::*;
        match (self, other) {
            (Unknown, t) | (t, Unknown) => t,
            (a, b) if a == b => a,
            (Int, Float) | (Float, Int) => Float,
            _ => Str,
        }
    }
}

fn classify(field: &str) -> Ty {
    if field.is_empty() {
        return Ty::Unknown; // missing constrains nothing
    }
    if field == "true" || field == "false" {
        return Ty::Bool;
    }
    if field.parse::<i64>().is_ok() {
        return Ty::Int;
    }
    if field.parse::<f64>().is_ok() {
        return Ty::Float;
    }
    Ty::Str
}

/// Pass 1: record spans `(start, end)` (end excludes the terminator), honoring
/// quote parity — `""` inside a quoted field toggles twice and lands back
/// in-quote, so a plain toggle is exact for boundary purposes.
///
/// The scan runs in TWO PARALLEL SWEEPS over fixed chunks: sweep one counts
/// each chunk's quotes, a serial prefix-xor then hands every chunk its
/// starting parity, and sweep two collects each chunk's parity-zero newlines.
/// Fixed boundaries + in-order stitching make the result identical to the
/// serial walk — thread count can never change it.
fn record_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    const CHUNK: usize = 1 << 20;
    let newlines: Vec<usize> = if bytes.len() <= CHUNK {
        let mut out = Vec::new();
        let mut in_quotes = false;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'"' => in_quotes = !in_quotes,
                b'\n' if !in_quotes => out.push(i),
                _ => {}
            }
        }
        out
    } else {
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let odd_quotes: Vec<bool> = chunks
            .par_iter()
            .map(|c| c.iter().filter(|&&b| b == b'"').count() % 2 == 1)
            .collect();
        let mut parity = Vec::with_capacity(chunks.len());
        let mut p = false;
        for q in &odd_quotes {
            parity.push(p);
            p ^= q;
        }
        chunks
            .par_iter()
            .enumerate()
            .flat_map_iter(|(k, c)| {
                let mut in_quotes = parity[k];
                let base = k * CHUNK;
                let mut out = Vec::new();
                for (i, &b) in c.iter().enumerate() {
                    match b {
                        b'"' => in_quotes = !in_quotes,
                        b'\n' if !in_quotes => out.push(base + i),
                        _ => {}
                    }
                }
                out
            })
            .collect()
    };
    let mut bounds = Vec::with_capacity(newlines.len() + 1);
    let mut start = 0usize;
    for &i in &newlines {
        let mut end = i;
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        bounds.push((start, end));
        start = i + 1;
    }
    if start < bytes.len() {
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        bounds.push((start, end));
    }
    bounds
}

/// Split one record into field BODY spans `(start, end, has_escape)` without
/// allocating — the structural twin of `parse_fields`, byte-for-byte the same
/// error messages in the same order.
fn split_fields(rec: &[u8], out: &mut Vec<(usize, usize, bool)>) -> Result<(), String> {
    out.clear();
    if rec.is_empty() {
        return Ok(());
    }
    let mut i = 0usize;
    loop {
        if i < rec.len() && rec[i] == b'"' {
            i += 1;
            let body = i;
            let mut has_esc = false;
            loop {
                match rec.get(i) {
                    Some(b'"') if rec.get(i + 1) == Some(&b'"') => {
                        has_esc = true;
                        i += 2;
                    }
                    Some(b'"') => break,
                    Some(_) => i += 1,
                    None => return Err("unterminated quoted field".to_string()),
                }
            }
            // Validate here so a non-UTF-8 field errors in the structural
            // pass, exactly where `parse_fields` would have said it.
            std::str::from_utf8(&rec[body..i]).map_err(|_| "non-UTF-8 bytes".to_string())?;
            out.push((body, i, has_esc));
            i += 1;
        } else {
            let end = rec[i..].iter().position(|&b| b == b',').map(|p| i + p).unwrap_or(rec.len());
            std::str::from_utf8(&rec[i..end]).map_err(|_| "non-UTF-8 bytes".to_string())?;
            out.push((i, end, false));
            i = end;
        }
        match rec.get(i) {
            Some(b',') => i += 1,
            None => return Ok(()),
            Some(_) => return Err("stray bytes after a closing quote".to_string()),
        }
    }
}

/// A field span's text: borrowed straight from the record, or — only when the
/// field carried a `""` escape — folded into `scratch`.
fn field_text<'a>(
    rec: &'a [u8],
    span: (usize, usize, bool),
    scratch: &'a mut String,
) -> Result<&'a str, String> {
    let (s, e, esc) = span;
    let body = std::str::from_utf8(&rec[s..e]).map_err(|_| "non-UTF-8 bytes".to_string())?;
    if !esc {
        return Ok(body);
    }
    scratch.clear();
    scratch.reserve(body.len());
    let mut rest = body;
    while let Some(p) = rest.find("\"\"") {
        scratch.push_str(&rest[..p + 1]);
        rest = &rest[p + 2..];
    }
    scratch.push_str(rest);
    Ok(scratch)
}

/// Parse one record's fields. Unquoted fields borrow-copy directly; a quoted
/// field unescapes `""`. Returns an error message (the caller adds row context).
fn parse_fields(rec: &[u8], out: &mut Vec<String>) -> Result<(), String> {
    out.clear();
    if rec.is_empty() {
        return Ok(());
    }
    let mut i = 0usize;
    loop {
        if i < rec.len() && rec[i] == b'"' {
            // Quoted field: scan to the closing quote, folding "" to ".
            let mut field = Vec::new();
            i += 1;
            loop {
                match rec.get(i) {
                    Some(b'"') if rec.get(i + 1) == Some(&b'"') => {
                        field.push(b'"');
                        i += 2;
                    }
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(&b) => {
                        field.push(b);
                        i += 1;
                    }
                    None => return Err("unterminated quoted field".to_string()),
                }
            }
            out.push(String::from_utf8(field).map_err(|_| "non-UTF-8 bytes".to_string())?);
        } else {
            let end = rec[i..].iter().position(|&b| b == b',').map(|p| i + p).unwrap_or(rec.len());
            let s = std::str::from_utf8(&rec[i..end]).map_err(|_| "non-UTF-8 bytes")?;
            out.push(s.to_string());
            i = end;
        }
        match rec.get(i) {
            Some(b',') => i += 1,
            None => break,
            Some(_) => return Err("stray bytes after a closing quote".to_string()),
        }
    }
    Ok(())
}

/// One column's typed segment, built by a chunk worker.
enum Seg {
    I64(Vec<i64>, Vec<bool>),
    F64(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Str { dict: Vec<String>, index: std::collections::HashMap<String, u32>, codes: Vec<u32>, valid: Vec<bool> },
}

impl Seg {
    fn new(ty: Ty, cap: usize) -> Seg {
        match ty {
            Ty::Int => Seg::I64(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Float => Seg::F64(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Bool => Seg::Bool(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Unknown | Ty::Str => Seg::Str {
                dict: Vec::new(),
                index: std::collections::HashMap::new(),
                codes: Vec::with_capacity(cap),
                valid: Vec::with_capacity(cap),
            },
        }
    }

    fn push(&mut self, field: &str, ctx: &dyn Fn(&str, &str) -> String) -> Result<(), String> {
        match self {
            Seg::I64(vals, valid) => {
                if field.is_empty() {
                    vals.push(0);
                    valid.push(false);
                } else {
                    vals.push(field.parse::<i64>().map_err(|_| ctx(field, "an integer"))?);
                    valid.push(true);
                }
            }
            Seg::F64(vals, valid) => {
                if field.is_empty() {
                    vals.push(0.0);
                    valid.push(false);
                } else {
                    vals.push(field.parse::<f64>().map_err(|_| ctx(field, "a number"))?);
                    valid.push(true);
                }
            }
            Seg::Bool(vals, valid) => match field {
                "" => {
                    vals.push(false);
                    valid.push(false);
                }
                "true" => {
                    vals.push(true);
                    valid.push(true);
                }
                "false" => {
                    vals.push(false);
                    valid.push(true);
                }
                other => return Err(ctx(other, "true or false")),
            },
            Seg::Str { dict, index, codes, valid } => {
                if field.is_empty() {
                    codes.push(0);
                    valid.push(false);
                } else {
                    let code = match index.get(field) {
                        Some(&c) => c,
                        None => {
                            let c = dict.len() as u32;
                            dict.push(field.to_string());
                            index.insert(field.to_string(), c);
                            c
                        }
                    };
                    codes.push(code);
                    valid.push(true);
                }
            }
        }
        Ok(())
    }
}

pub fn read_csv(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let bytes =
        std::fs::read(path).map_err(|e| err(format!("could not open CSV `{path}`: {e}")))?;
    // Strip a UTF-8 BOM so the first header name doesn't carry it invisibly.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(&bytes[..]);

    let bounds = record_bounds(bytes);
    if bounds.is_empty() {
        return Err(err(format!("CSV `{path}` is empty — a header row is required")));
    }
    let mut fields = Vec::new();
    parse_fields(&bytes[bounds[0].0..bounds[0].1], &mut fields)
        .map_err(|m| err(format!("could not parse CSV `{path}` header: {m}")))?;
    let headers: Vec<String> = std::mem::take(&mut fields);
    let ncol = headers.len();
    let rows = &bounds[1..];

    // Inference window (serial — at most INFER_ROWS records, re-parsed cheaply
    // again by the parallel pass).
    let mut tys = vec![Ty::Unknown; ncol];
    for (r, &(s, e)) in rows.iter().take(INFER_ROWS).enumerate() {
        parse_fields(&bytes[s..e], &mut fields)
            .map_err(|m| err(format!("could not parse CSV `{path}` row {}: {m}", r + 2)))?;
        for (c, f) in fields.iter().enumerate().take(ncol) {
            tys[c] = tys[c].join(classify(f));
        }
    }
    let tys: Vec<Ty> =
        tys.into_iter().map(|t| if t == Ty::Unknown { Ty::Str } else { t }).collect();

    // Parallel pass: fixed row ranges -> typed segments, spliced in order. The
    // chunk size keeps per-thread work meaningful on small files.
    let chunk = (rows.len() / (rayon::current_num_threads() * 4).max(1)).max(4096);
    let results: Vec<Result<Vec<Seg>, (usize, String)>> = rows
        .par_chunks(chunk)
        .enumerate()
        .map(|(ci, spans)| {
            let base = ci * chunk; // first row index of this chunk (0-based data row)
            let mut segs: Vec<Seg> =
                tys.iter().map(|t| Seg::new(*t, spans.len())).collect();
            let mut fspans: Vec<(usize, usize, bool)> = Vec::with_capacity(ncol);
            let mut scratch = String::new();
            for (r, &(s, e)) in spans.iter().enumerate() {
                let row = base + r;
                let rec = &bytes[s..e];
                split_fields(rec, &mut fspans).map_err(|m| (row, m))?;
                if fspans.len() != ncol {
                    return Err((
                        row,
                        format!("has {} fields, expected {ncol}", fspans.len()),
                    ));
                }
                for (c, &span) in fspans.iter().enumerate() {
                    let name = &headers[c];
                    let ctx = |field: &str, want: &str| {
                        format!(
                            "column `{name}`: `{field}` is not {want} (the column's type \
                             was inferred from the first {INFER_ROWS} rows)"
                        )
                    };
                    let f = field_text(rec, span, &mut scratch).map_err(|m| (row, m))?;
                    segs[c].push(f, &ctx).map_err(|m| (row, m))?;
                }
            }
            Ok(segs)
        })
        .collect();

    // Earliest-row error wins, independent of which thread found what first.
    if let Some((row, m)) =
        results.iter().filter_map(|r| r.as_ref().err()).min_by_key(|(row, _)| *row)
    {
        return Err(err(format!("CSV `{path}` row {}: {m}", row + 2)));
    }

    // Splice: typed columns extend; string columns hash-cons into their
    // dictionary (chunk workers hand over plain Strings — Send — and the
    // builder dedups here, single-threaded but per-distinct, not per-cell).
    enum Acc {
        I(Vec<i64>, Vec<bool>),
        F(Vec<f64>, Vec<bool>),
        B(Vec<bool>, Vec<bool>),
        S(super::columns::StrBuilder),
    }
    let n = rows.len();
    let mut accs: Vec<Acc> = tys
        .iter()
        .map(|ty| match ty {
            Ty::Int => Acc::I(Vec::with_capacity(n), Vec::with_capacity(n)),
            Ty::Float => Acc::F(Vec::with_capacity(n), Vec::with_capacity(n)),
            Ty::Bool => Acc::B(Vec::with_capacity(n), Vec::with_capacity(n)),
            Ty::Unknown | Ty::Str => Acc::S(super::columns::StrBuilder::with_capacity(n)),
        })
        .collect();
    for segs in results.into_iter().flatten() {
        for (acc, seg) in accs.iter_mut().zip(segs) {
            match (acc, seg) {
                (Acc::I(vals, valid), Seg::I64(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Acc::F(vals, valid), Seg::F64(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Acc::B(vals, valid), Seg::Bool(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Acc::S(b), Seg::Str { dict, codes, valid, .. }) => {
                    // Remap: per-chunk dict entry -> global code, once per
                    // DISTINCT value; the 5M cells are integer rewrites.
                    let trans: Vec<u32> = dict.iter().map(|s| b.intern(s)).collect();
                    for (code, ok) in codes.iter().zip(valid) {
                        if ok {
                            b.push_code(trans[*code as usize]);
                        } else {
                            b.push_missing();
                        }
                    }
                }
                _ => {
                    return Err(err("internal: CSV segment dtype drift".to_string()));
                }
            }
        }
    }
    let mut cols: Vec<(String, Col)> = headers
        .iter()
        .zip(accs)
        .map(|(name, acc)| {
            let c = match acc {
                Acc::I(vals, valid) => Col::I64 { vals, valid },
                Acc::F(vals, valid) => Col::F64 { vals, valid },
                Acc::B(vals, valid) => Col::Bool { vals, valid },
                Acc::S(b) => b.finish(),
            };
            (name.clone(), c)
        })
        .collect();
    // An all-missing column reads as Str today (empty fields constrain nothing);
    // normalize to the Null column the rest of the engine uses for "no evidence".
    for (_, c) in cols.iter_mut() {
        if let Col::Str { codes, valid, .. } = c
            && !valid.is_empty()
            && valid.iter().all(|v| !v)
        {
            *c = Col::Null { len: codes.len() };
        }
    }
    NativeFrame::new(cols, line, col).map(|f| Rc::new(f) as crate::backend::Df)
}

pub fn write_csv(
    frame: &NativeFrame,
    path: &str,
    sep: u8,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    use std::io::Write as _;
    let err = |m: String| HelixError::new(m, line, col);

    // Hand-rolled, chunk-parallel writer. Field text is typed straight from the
    // columns (no Value, no per-cell String); quoting is the standard
    // when-necessary rule (field contains the separator, a quote, or a line
    // break). Chunks of rows format in parallel and concatenate in order, so
    // the bytes are identical at any thread count. Floats keep their point
    // (`2.0`, fmt_float's exact text) so a round-trip re-infers the dtype.
    let n = frame.len();
    let frame_cols = frame.columns(line, col)?;
    let mut head = Vec::new();
    for (i, (name, _)) in frame_cols.iter().enumerate() {
        if i > 0 {
            head.push(sep);
        }
        push_field(&mut head, name.as_bytes(), sep);
    }
    head.extend_from_slice(b"\n");

    // Send-able views: an `Rc<String>` cannot cross threads, but `&str`
    // borrowed out of one can — collect the borrow per string column once.
    enum View<'a> {
        I(&'a [i64], &'a [bool]),
        F(&'a [f64], &'a [bool]),
        B(&'a [bool], &'a [bool]),
        S(Vec<&'a str>, &'a [u32], &'a [bool]),
        N,
    }
    let views: Vec<View> = frame_cols
        .iter()
        .map(|(_, c)| match *c {
            Col::I64 { vals, valid } => View::I(vals, valid),
            Col::F64 { vals, valid } => View::F(vals, valid),
            Col::Bool { vals, valid } => View::B(vals, valid),
            Col::Str { dict, codes, valid } => {
                View::S(dict.iter().map(|s| s.as_str()).collect(), codes, valid)
            }
            Col::Null { .. } => View::N,
        })
        .collect();

    let chunk = (n / (rayon::current_num_threads() * 4).max(1)).max(8192);
    let starts: Vec<usize> = (0..n).step_by(chunk).collect();
    let blocks: Vec<Vec<u8>> = starts
        .par_iter()
        .map(|&start| {
            let end = (start + chunk).min(n);
            let mut buf = Vec::with_capacity((end - start) * 24);
            let mut scratch = String::new();
            for row in start..end {
                for (i, view) in views.iter().enumerate() {
                    if i > 0 {
                        buf.push(sep);
                    }
                    match view {
                        View::I(vals, valid) => {
                            if valid[row] {
                                use std::fmt::Write as _;
                                scratch.clear();
                                let _ = write!(scratch, "{}", vals[row]);
                                buf.extend_from_slice(scratch.as_bytes());
                            }
                        }
                        View::F(vals, valid) => {
                            if valid[row] {
                                use std::fmt::Write as _;
                                scratch.clear();
                                let x = vals[row];
                                if x.is_finite() && x == x.trunc() {
                                    let _ = write!(scratch, "{x:.1}");
                                } else {
                                    let _ = write!(scratch, "{x}");
                                }
                                buf.extend_from_slice(scratch.as_bytes());
                            }
                        }
                        View::B(vals, valid) => {
                            if valid[row] {
                                buf.extend_from_slice(if vals[row] {
                                    b"true"
                                } else {
                                    b"false"
                                });
                            }
                        }
                        View::S(dict, codes, valid) => {
                            if valid[row] {
                                push_field(&mut buf, dict[codes[row] as usize].as_bytes(), sep);
                            }
                        }
                        View::N => {}
                    }
                }
                buf.push(b'\n');
            }
            buf
        })
        .collect();

    let file = std::fs::File::create(path)
        .map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    out.write_all(&head).map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    for b in &blocks {
        out.write_all(b).map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    }
    out.flush().map_err(|e| err(format!("could not write `{path}`: {e}")))
}

/// One text field with when-necessary quoting (quote doubled inside quotes).
/// Numbers and booleans never need it — only callers with real text use this.
fn push_field(buf: &mut Vec<u8>, field: &[u8], sep: u8) {
    let needs =
        field.iter().any(|&b| b == sep || b == b'"' || b == b'\n' || b == b'\r');
    if !needs {
        buf.extend_from_slice(field);
        return;
    }
    buf.push(b'"');
    for &b in field {
        if b == b'"' {
            buf.push(b'"');
        }
        buf.push(b);
    }
    buf.push(b'"');
}
