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
fn record_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                let mut end = i;
                if end > start && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                bounds.push((start, end));
                start = i + 1;
            }
            _ => {}
        }
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
    Str(Vec<String>, Vec<bool>),
}

impl Seg {
    fn new(ty: Ty, cap: usize) -> Seg {
        match ty {
            Ty::Int => Seg::I64(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Float => Seg::F64(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Bool => Seg::Bool(Vec::with_capacity(cap), Vec::with_capacity(cap)),
            Ty::Unknown | Ty::Str => {
                Seg::Str(Vec::with_capacity(cap), Vec::with_capacity(cap))
            }
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
            Seg::Str(vals, valid) => {
                if field.is_empty() {
                    vals.push(String::new());
                    valid.push(false);
                } else {
                    vals.push(field.to_string());
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
            let mut fields: Vec<String> = Vec::with_capacity(ncol);
            for (r, &(s, e)) in spans.iter().enumerate() {
                let row = base + r;
                parse_fields(&bytes[s..e], &mut fields).map_err(|m| (row, m))?;
                if fields.len() != ncol {
                    return Err((
                        row,
                        format!("has {} fields, expected {ncol}", fields.len()),
                    ));
                }
                for (c, f) in fields.iter().enumerate() {
                    let name = &headers[c];
                    let ctx = |field: &str, want: &str| {
                        format!(
                            "column `{name}`: `{field}` is not {want} (the column's type \
                             was inferred from the first {INFER_ROWS} rows)"
                        )
                    };
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

    let mut cols: Vec<(String, Col)> = headers
        .iter()
        .zip(&tys)
        .map(|(name, ty)| {
            let n = rows.len();
            let c = match ty {
                Ty::Int => Col::I64 { vals: Vec::with_capacity(n), valid: Vec::with_capacity(n) },
                Ty::Float => {
                    Col::F64 { vals: Vec::with_capacity(n), valid: Vec::with_capacity(n) }
                }
                Ty::Bool => {
                    Col::Bool { vals: Vec::with_capacity(n), valid: Vec::with_capacity(n) }
                }
                Ty::Unknown | Ty::Str => {
                    Col::Str { vals: Vec::with_capacity(n), valid: Vec::with_capacity(n) }
                }
            };
            (name.clone(), c)
        })
        .collect();
    for segs in results.into_iter().flatten() {
        for (slot, seg) in cols.iter_mut().zip(segs) {
            match (&mut slot.1, seg) {
                (Col::I64 { vals, valid }, Seg::I64(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Col::F64 { vals, valid }, Seg::F64(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Col::Bool { vals, valid }, Seg::Bool(v, m)) => {
                    vals.extend(v);
                    valid.extend(m);
                }
                (Col::Str { vals, valid }, Seg::Str(v, m)) => {
                    vals.extend(v.into_iter().map(Rc::new));
                    valid.extend(m);
                }
                _ => {
                    return Err(err("internal: CSV segment dtype drift".to_string()));
                }
            }
        }
    }
    // An all-missing column reads as Str today (empty fields constrain nothing);
    // normalize to the Null column the rest of the engine uses for "no evidence".
    for (_, c) in cols.iter_mut() {
        if let Col::Str { vals, valid } = c
            && !valid.is_empty()
            && valid.iter().all(|v| !v)
        {
            *c = Col::Null { len: vals.len() };
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
    let mut head = Vec::new();
    for (i, (name, _)) in frame.columns().iter().enumerate() {
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
        S(Vec<&'a str>, &'a [bool]),
        N,
    }
    let views: Vec<View> = frame
        .columns()
        .iter()
        .map(|(_, c)| match c {
            Col::I64 { vals, valid } => View::I(vals, valid),
            Col::F64 { vals, valid } => View::F(vals, valid),
            Col::Bool { vals, valid } => View::B(vals, valid),
            Col::Str { vals, valid } => {
                View::S(vals.iter().map(|s| s.as_str()).collect(), valid)
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
                        View::S(vals, valid) => {
                            if valid[row] {
                                push_field(&mut buf, vals[row].as_bytes(), sep);
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
