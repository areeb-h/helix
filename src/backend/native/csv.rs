//! CSV in and out (ADR 0034 §8): header row, dtype inference over the first 100
//! records (Int ⊂ Float ⊂ Str; `true`/`false` make Bool; empty is missing; no
//! date parsing), RFC 4180 quoting via the `csv` crate both directions. Corner
//! behavior is pinned by oracle tests, never guessed.

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

pub fn read_csv(path: &str, line: usize, col: usize) -> Result<crate::backend::Df, HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let open = || {
        csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(false)
            .from_path(path)
            .map_err(|e| err(format!("could not open CSV `{path}`: {e}")))
    };

    // Pass 1: headers + the inference window.
    let mut rdr = open()?;
    let headers: Vec<String> =
        rdr.headers().map_err(|e| err(format!("could not read CSV `{path}`: {e}")))?
            .iter()
            .map(str::to_string)
            .collect();
    let ncol = headers.len();
    let mut tys = vec![Ty::Unknown; ncol];
    for (i, rec) in rdr.records().enumerate() {
        if i >= INFER_ROWS {
            break;
        }
        let rec = rec.map_err(|e| err(format!("could not parse CSV `{path}`: {e}")))?;
        for (c, field) in rec.iter().enumerate().take(ncol) {
            tys[c] = tys[c].join(classify(field));
        }
    }

    // Pass 2: parse into typed columns. A post-window value that contradicts the
    // committed dtype is a clean error naming the row — never a silent string
    // column or a lost value.
    let mut rdr = open()?;
    let mut cells: Vec<Vec<Option<String>>> = vec![Vec::new(); ncol];
    for rec in rdr.records() {
        let rec = rec.map_err(|e| err(format!("could not parse CSV `{path}`: {e}")))?;
        if rec.len() != ncol {
            return Err(err(format!(
                "CSV `{path}` row {} has {} fields, expected {ncol}",
                cells[0].len() + 2, // 1-based, after the header line
                rec.len()
            )));
        }
        for (c, field) in rec.iter().enumerate() {
            cells[c].push(if field.is_empty() { None } else { Some(field.to_string()) });
        }
    }

    let mut cols: Vec<(String, Col)> = Vec::with_capacity(ncol);
    for (c, name) in headers.iter().enumerate() {
        let ty = if tys[c] == Ty::Unknown { Ty::Str } else { tys[c] };
        let column = build_column(name, ty, &cells[c], path).map_err(err)?;
        cols.push((name.clone(), column));
    }
    NativeFrame::new(cols, line, col).map(|f| std::rc::Rc::new(f) as crate::backend::Df)
}

fn build_column(
    name: &str,
    ty: Ty,
    raw: &[Option<String>],
    path: &str,
) -> Result<Col, String> {
    let n = raw.len();
    let bad = |row: usize, field: &str, want: &str| {
        format!(
            "CSV `{path}` column `{name}` row {}: `{field}` is not {want} \
             (the column's type was inferred from the first {INFER_ROWS} rows)",
            row + 2
        )
    };
    Ok(match ty {
        Ty::Int => {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for (i, f) in raw.iter().enumerate() {
                match f {
                    None => {
                        vals.push(0);
                        valid.push(false);
                    }
                    Some(s) => {
                        vals.push(s.parse::<i64>().map_err(|_| bad(i, s, "an integer"))?);
                        valid.push(true);
                    }
                }
            }
            Col::I64 { vals, valid }
        }
        Ty::Float => {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for (i, f) in raw.iter().enumerate() {
                match f {
                    None => {
                        vals.push(0.0);
                        valid.push(false);
                    }
                    Some(s) => {
                        vals.push(s.parse::<f64>().map_err(|_| bad(i, s, "a number"))?);
                        valid.push(true);
                    }
                }
            }
            Col::F64 { vals, valid }
        }
        Ty::Bool => {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for (i, f) in raw.iter().enumerate() {
                match f.as_deref() {
                    None => {
                        vals.push(false);
                        valid.push(false);
                    }
                    Some("true") => {
                        vals.push(true);
                        valid.push(true);
                    }
                    Some("false") => {
                        vals.push(false);
                        valid.push(true);
                    }
                    Some(s) => return Err(bad(i, s, "true or false")),
                }
            }
            Col::Bool { vals, valid }
        }
        Ty::Str | Ty::Unknown => {
            let mut vals = Vec::with_capacity(n);
            let mut valid = Vec::with_capacity(n);
            for f in raw {
                match f {
                    None => {
                        vals.push(String::new());
                        valid.push(false);
                    }
                    Some(s) => {
                        vals.push(s.clone());
                        valid.push(true);
                    }
                }
            }
            Col::Str { vals, valid }
        }
    })
}

pub fn write_csv(
    frame: &NativeFrame,
    path: &str,
    sep: u8,
    line: usize,
    col: usize,
) -> Result<(), HelixError> {
    let err = |m: String| HelixError::new(m, line, col);
    let mut w = csv::WriterBuilder::new()
        .delimiter(sep)
        .from_path(path)
        .map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    let names: Vec<&str> = frame.columns().iter().map(|(n, _)| n.as_str()).collect();
    w.write_record(&names).map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    for row in 0..frame.len() {
        let rec: Vec<String> =
            frame.columns().iter().map(|(_, c)| cell_csv(&c.get(row))).collect();
        w.write_record(&rec).map_err(|e| err(format!("could not write `{path}`: {e}")))?;
    }
    w.flush().map_err(|e| err(format!("could not write `{path}`: {e}")))
}

/// A cell's CSV text: missing is an empty field; floats keep their point
/// (`2.0`) so a round-trip re-infers the same dtype.
fn cell_csv(v: &crate::value::Value) -> String {
    use crate::value::Value;
    match v {
        Value::Missing => String::new(),
        Value::Float(x) => crate::value::fmt_float(*x),
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    }
}
