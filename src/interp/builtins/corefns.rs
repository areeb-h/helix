//! Builtins: core conversions and constructors (chr/ord/to_int/dict/range) — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_range(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match args.len() {
        1 => {
            let n = as_int(&args[0], "range", line, col)?;
            int_range(0, n, line, col)
        }
        2 => {
            let a = as_int(&args[0], "range", line, col)?;
            let b = as_int(&args[1], "range", line, col)?;
            int_range(a, b, line, col)
        }
        3 => {
            let a = as_int(&args[0], "range", line, col)?;
            let b = as_int(&args[1], "range", line, col)?;
            let step = as_int(&args[2], "range", line, col)?;
            int_range_step(a, b, step, line, col)
        }
        _ => Err(HelixError::new(
            format!("`range` takes 1 to 3 arguments, got {}", args.len()),
            line,
            col,
        )
        .hint("use `range(n)`, `range(start, stop)`, or `range(start, stop, step)`.")),
    }
}

#[inline]
pub(super) fn a_chr(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        if matches!(args[0], Value::Missing) {
            return Ok(Value::Missing);
        }
        let cp = as_int(&args[0], "chr", line, col)?;
        match u32::try_from(cp).ok().and_then(char::from_u32) {
            Some(c) => Ok(Value::Str(Rc::new(c.to_string()))),
            None => Err(HelixError::new(
                format!("`chr` got {cp}, which is not a valid Unicode codepoint"),
                line,
                col,
            )
            .hint("pass 0..=1114111 (0x10FFFF), excluding the surrogate range.")),
        }
    
}

#[inline]
pub(super) fn a_ord(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Missing => Ok(Value::Missing),
            // The codepoint of the FIRST character (forgiving on longer strings).
            Value::Str(s) | Value::Dna(s) => match s.chars().next() {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Err(HelixError::new("`ord` got an empty string", line, col)
                    .hint("pass a one-character string, e.g. `ord(\"A\")`.")),
            },
            other => Err(type_err("ord", "a single-character string", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_to_float(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        use num_traits::ToPrimitive;
        match &args[0] {
            Value::Int(i) => Ok(Value::Float(*i as f64)),
            Value::Float(f) => Ok(Value::Float(*f)),
            Value::Rational(r) => Ok(Value::Float(r.to_f64().unwrap_or(f64::NAN))),
            // A numeric *string* is parsed (the honest spelling of what people
            // used to reach for `parse_json` to do).
            Value::Str(s) => parse_str_float(s, line, col),
            Value::Missing => Ok(Value::Missing),
            other => Err(type_err("to_float", "a number or numeric string", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_dict(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // An empty keyed map (ADR 0020); build a populated one with
        // `[(k, v), …].to_dict()`. Grows via the immutable `.insert(k, v)`.
        if !args.is_empty() {
            return Err(HelixError::new(
                "`dict()` takes no arguments",
                line,
                col,
            )
            .hint("build a populated dict with `[(k, v), …].to_dict()` or `xs.frequencies().to_dict()`."));
        }
        Ok(Value::Dict(Rc::new(std::collections::BTreeMap::new())))
    
}

#[inline]
pub(super) fn a_to_int(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        use num_traits::ToPrimitive;
        match &args[0] {
            Value::Int(i) => Ok(Value::Int(*i)),
            // Truncate toward zero — matches the usual numeric→int narrowing.
            Value::Float(f) => Ok(Value::Int(f.trunc() as i64)),
            Value::Rational(r) => Ok(Value::Int(r.to_f64().unwrap_or(f64::NAN).trunc() as i64)),
            Value::Str(s) => parse_str_int(s, line, col),
            Value::Missing => Ok(Value::Missing),
            other => Err(type_err("to_int", "a number or integer string", other, line, col)),
        }
    
}
