//! Builtins: the reverse-mode autodiff entry points — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.


use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "variable" | "value_of" | "gradient") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "variable" => {
                    arity(name, &args, 1, line, col)?;
                    crate::autodiff::variable(&args[0], line, col)
                }
                "value_of" => {
                    arity(name, &args, 1, line, col)?;
                    Ok(crate::autodiff::value_of(&args[0]))
                }
                "gradient" => {
                    arity(name, &args, 2, line, col)?;
                    crate::autodiff::gradient(&args[0], &args[1], line, col)
                }
                // ---- math standard library (broadcasts over arrays, propagates missing) ----
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}
