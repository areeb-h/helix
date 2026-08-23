//! Builtins: the reverse-mode autodiff entry points — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.


use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_variable(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        crate::autodiff::variable(&args[0], line, col)
    
}

#[inline]
pub(super) fn a_value_of(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        Ok(crate::autodiff::value_of(&args[0]))
    
}

#[inline]
pub(super) fn a_gradient(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 2, line, col)?;
        crate::autodiff::gradient(&args[0], &args[1], line, col)
    
}

// ---- math standard library (broadcasts over arrays, propagates missing) ----
