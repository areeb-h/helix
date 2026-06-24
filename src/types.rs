//! Static type inference & checking (Phase 5, ADR-0002).
//!
//! Bidirectional, localized inference. **Permissive**: an error is emitted ONLY
//! when two concrete types are provably incompatible *and the runtime would also
//! fail*. Everything unprovable (DataFrame columns, dynamic/mixed data) becomes
//! the top type `Unknown`, which is compatible with everything and never errors.
//! The hard requirement is **zero false positives** — a program that runs today
//! must never be rejected.
//!
//! The pass runs after parse, before interpretation (see `main.rs`). It is
//! compile-time only — no runtime contracts — so it sidesteps the gradual-typing
//! performance cliff documented in ADR-0002.

use std::fmt;

use rustc_hash::FxHashMap;

use crate::ast::{BinOp, Expr, Stmt, TypeAnn, UnOp};
use crate::error::{suggest, HelixError};

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    /// "some number" — Int or Float, statically unresolved (`Int**Int`,
    /// `min/max`, `Array.sum()`). Compatible with both Int and Float.
    Num,
    String,
    Bool,
    Array(Box<Type>),
    /// A fixed-size tuple type, element types in order.
    Tuple(Vec<Type>),
    /// An ordered record type carrying its field names + types.
    Record(Vec<(String, Type)>),
    Tensor,
    DataFrame,
    GroupBy,
    Dna,
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    Unit,
    /// Absent data. BOTTOM: compatible with everything; drops under `join`.
    Missing,
    /// Permissive TOP (Any/Dynamic): compatible with everything; NEVER errors.
    Unknown,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::Float => write!(f, "Float"),
            Type::Num => write!(f, "Num"),
            Type::String => write!(f, "String"),
            Type::Bool => write!(f, "Bool"),
            Type::Array(_) => write!(f, "Array"),
            Type::Tuple(_) => write!(f, "Tuple"),
            Type::Record(_) => write!(f, "Record"),
            Type::Tensor => write!(f, "Tensor"),
            Type::DataFrame => write!(f, "DataFrame"),
            Type::GroupBy => write!(f, "GroupBy"),
            Type::Dna => write!(f, "Dna"),
            Type::Function { .. } => write!(f, "Function"),
            Type::Unit => write!(f, "Unit"),
            Type::Missing => write!(f, "Missing"),
            Type::Unknown => write!(f, "Unknown"),
        }
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float | Type::Num)
}

fn array_of_unknown() -> Type {
    Type::Array(Box::new(Type::Unknown))
}

pub fn ann_to_type(a: &TypeAnn) -> Type {
    match a {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Num => Type::Num,
        TypeAnn::String => Type::String,
        TypeAnn::Bool => Type::Bool,
        TypeAnn::Array => array_of_unknown(),
        TypeAnn::Tensor => Type::Tensor,
        TypeAnn::DataFrame => Type::DataFrame,
        TypeAnn::Dna => Type::Dna,
    }
}

/// The ONLY source of type errors. Symmetric. `Unknown`/`Missing` are compatible
/// with everything; numerics form one tower; `Array`/`Function` are structural.
pub fn compatible(a: &Type, b: &Type) -> bool {
    use Type::*;
    match (a, b) {
        (Unknown, _) | (_, Unknown) => true,
        (Missing, _) | (_, Missing) => true,
        _ if a == b => true,
        _ if is_numeric(a) && is_numeric(b) => true,
        (Array(x), Array(y)) => compatible(x, y),
        (Tuple(a), Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| compatible(x, y))
        }
        (Record(_), Record(_)) => true, // permissive; field access does the checking

        (
            Function { params: p1, ret: r1 },
            Function { params: p2, ret: r2 },
        ) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2.iter()).all(|(x, y)| compatible(x, y))
                && compatible(r1, r2)
        }
        _ => false,
    }
}

/// Least-upper-bound. TOTAL — never errors; incompatible pairs widen to
/// `Unknown`. Used for `if` branches and array elements so they never reject.
pub fn join(a: &Type, b: &Type) -> Type {
    use Type::*;
    match (a, b) {
        _ if a == b => a.clone(),
        (Unknown, _) | (_, Unknown) => Unknown,
        (Missing, t) | (t, Missing) => t.clone(),
        (Int, Float) | (Float, Int) | (Num, _) | (_, Num) if is_numeric(a) && is_numeric(b) => Num,
        (Array(x), Array(y)) => Array(Box::new(join(x, y))),
        (Tuple(a), Tuple(b)) if a.len() == b.len() => {
            Tuple(a.iter().zip(b).map(|(x, y)| join(x, y)).collect())
        }
        _ => Unknown,
    }
}

/// Result type of `l <op> r` for two scalar numeric operands — mirrors the
/// runtime `arith`/`eval_binary` exactly.
fn arith_result(op: &BinOp, l: &Type, r: &Type) -> Type {
    use BinOp::*;
    let has_float = matches!(l, Type::Float) || matches!(r, Type::Float);
    let both_int = matches!(l, Type::Int) && matches!(r, Type::Int);
    match op {
        Div => Type::Float, // division is always Float
        Pow => {
            if both_int {
                Type::Num // Int**Int may overflow to Float
            } else if has_float {
                Type::Float
            } else {
                Type::Num
            }
        }
        // Add Sub Mul Mod
        _ => {
            if both_int {
                Type::Int
            } else if has_float {
                Type::Float
            } else {
                Type::Num
            }
        }
    }
}

/// Arithmetic over concrete, non-Unknown, non-Missing operands (those are
/// handled earlier). Returns None for a *provable* mismatch (caller errors).
fn arith_broadcast(op: &BinOp, l: &Type, r: &Type) -> Option<Type> {
    // tensor arithmetic (tensor with tensor or a scalar number)
    if matches!(l, Type::Tensor) || matches!(r, Type::Tensor) {
        let ok = |t: &Type| matches!(t, Type::Tensor) || is_numeric(t);
        return if ok(l) && ok(r) { Some(Type::Tensor) } else { None };
    }
    // array broadcasting (array with array or a scalar number)
    if matches!(l, Type::Array(_)) || matches!(r, Type::Array(_)) {
        let ok = |t: &Type| matches!(t, Type::Array(_)) || is_numeric(t);
        return if ok(l) && ok(r) {
            Some(array_of_unknown())
        } else {
            None
        };
    }
    // scalar arithmetic
    if is_numeric(l) && is_numeric(r) {
        Some(arith_result(op, l, r))
    } else {
        None
    }
}

// ---------- error helpers (mirror interp.rs wording exactly) ----------

fn type_err(who: &str, want: &str, got: &Type, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!("`{}` expected {}, found a value of type {}", who, want, got),
        line,
        col,
    )
}

fn arity_err(name: &str, want: usize, got: usize, line: usize, col: usize) -> HelixError {
    HelixError::new(
        format!(
            "`{}` takes {} argument{}, got {}",
            name,
            want,
            if want == 1 { "" } else { "s" },
            got
        ),
        line,
        col,
    )
}

/// Field access `x.name` on something that isn't a record. If `name` is actually
/// a method of that type, nudge the user to call it with `()`.
fn field_on_non_record(t: &Type, name: &str, line: usize, col: usize) -> HelixError {
    let methods: &[&str] = match t {
        Type::Array(_) => ARRAY_METHODS,
        Type::String => STRING_METHODS,
        Type::Dna => DNA_METHODS,
        Type::Tensor => TENSOR_METHODS,
        Type::DataFrame => DF_METHODS,
        Type::GroupBy => GROUPBY_AGGS,
        _ => &[],
    };
    let err = HelixError::new(
        format!("a value of type {} has no field `{}`", t, name),
        line,
        col,
    );
    if methods.contains(&name) || name == "is_missing" {
        err.hint(format!("`{}` is a method — call it with `{}()`.", name, name))
    } else {
        err.hint("field access `x.name` works on records; methods need `()`.")
    }
}

fn unknown_method(type_name: &str, name: &str, candidates: &[&str], line: usize, col: usize) -> HelixError {
    let mut err = HelixError::new(format!("type {} has no method `{}`", type_name, name), line, col);
    if let Some(s) = suggest(name, candidates) {
        err = err.hint(format!("did you mean `{}`?", s));
    } else {
        err = err.hint(format!("available {} methods: {}", type_name, candidates.join(", ")));
    }
    err
}

const ARRAY_METHODS: &[&str] = &[
    "mean", "std", "sum", "min", "max", "count", "normalize", "sort", "reverse", "first", "last",
    "map", "filter", "where", "reduce", "any", "all", "take", "drop", "zip", "enumerate", "top",
    "drop_missing", "is_missing",
];
const STRING_METHODS: &[&str] = &["upper", "lower", "count", "reverse", "is_missing"];
const DNA_METHODS: &[&str] = &[
    "gc_content",
    "reverse_complement",
    "complement",
    "kmers",
    "find",
    "length",
    "is_missing",
];
const TENSOR_METHODS: &[&str] = &[
    "shape", "ndim", "count", "sum", "mean", "min", "max", "flatten", "reshape", "transpose", "t",
    "matmul", "dot", "norm", "det", "inv", "solve", "is_missing",
];
const DF_METHODS: &[&str] = &[
    "where", "filter", "select", "sort", "group", "head", "count", "columns", "cache",
    "is_missing",
];
const GROUPBY_AGGS: &[&str] = &["mean", "sum", "min", "max", "count", "std", "is_missing"];

const BUILTIN_FNS: &[&str] = &[
    "print", "dna", "range", "read_csv", "read_parquet", "read_fasta", "write_parquet", "tensor",
    "zeros",
    "ones", "eye", "sqrt", "cbrt", "abs", "exp", "ln", "log10", "log2", "log", "sin", "cos", "tan",
    "asin", "acos", "atan", "atan2", "sinh", "cosh", "tanh", "floor", "ceil", "round", "trunc",
    "sign", "degrees", "radians", "hypot", "min", "max",
];

const MATH_UNARY_FLOAT: &[&str] = &[
    "sqrt", "cbrt", "exp", "ln", "log10", "log2", "sin", "cos", "tan", "asin", "acos", "atan",
    "sinh", "cosh", "tanh", "degrees", "radians",
];

// ---------- the checker ----------

/// Inferred type of each method *receiver*, keyed by the receiver expression's
/// node address. Built during checking and handed to the bytecode compiler so it
/// can route receiver-polymorphic methods (`where`/`sort`/`min`, which mean
/// different things for Array vs DataFrame vs Tensor) by the receiver's true type
/// instead of guessing from the method name. The keys are stable because the AST
/// is not cloned or moved between `types::check` and `bytecode::compile`.
pub type TypeMap = FxHashMap<*const Expr, Type>;

pub struct Checker {
    env: FxHashMap<String, Type>,
    /// Accumulated receiver types (see [`TypeMap`]).
    types: TypeMap,
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker {
    pub fn new() -> Self {
        let mut env = FxHashMap::default();
        env.insert("pi".to_string(), Type::Float);
        env.insert("e".to_string(), Type::Float);
        env.insert("inf".to_string(), Type::Float);
        Checker { env, types: FxHashMap::default() }
    }

    pub fn exec_stmt(&mut self, s: &Stmt) -> Result<(), HelixError> {
        match s {
            Stmt::Assign { name, value, .. } => {
                let t = self.synth(value)?;
                self.env.insert(name.clone(), t);
                Ok(())
            }
            Stmt::Destructure {
                names,
                value,
                line,
                col,
                ..
            } => {
                let t = self.synth(value)?;
                match &t {
                    Type::Tuple(els) => {
                        if els.len() != names.len() {
                            return Err(HelixError::new(
                                format!(
                                    "cannot destructure {} values into {} names",
                                    els.len(),
                                    names.len()
                                ),
                                *line,
                                *col,
                            ));
                        }
                        for (n, et) in names.iter().zip(els.iter()) {
                            self.env.insert(n.clone(), et.clone());
                        }
                    }
                    Type::Array(el) => {
                        // array length is dynamic — each name gets the element type
                        for n in names {
                            self.env.insert(n.clone(), (**el).clone());
                        }
                    }
                    Type::Unknown | Type::Missing => {
                        for n in names {
                            self.env.insert(n.clone(), Type::Unknown);
                        }
                    }
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "cannot destructure a value of type {} into {} names",
                                other,
                                names.len()
                            ),
                            *line,
                            *col,
                        )
                        .hint("the right-hand side must be a tuple or array."))
                    }
                }
                Ok(())
            }
            Stmt::Func {
                name,
                params,
                ret,
                body,
                line,
                col,
            } => self.check_func(name, params, ret, body, *line, *col),
            Stmt::Expr(e) => {
                self.synth(e)?;
                Ok(())
            }
            // Stripped by the module loader before type-checking (see `Stmt::Import`).
            Stmt::Import { .. } => Ok(()),
        }
    }

    fn check_func(
        &mut self,
        name: &str,
        params: &[(String, Option<TypeAnn>)],
        ret: &Option<TypeAnn>,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> Result<(), HelixError> {
        let param_types: Vec<Type> = params
            .iter()
            .map(|(_, ann)| ann.as_ref().map(ann_to_type).unwrap_or(Type::Unknown))
            .collect();
        let ret_ann = ret.as_ref().map(ann_to_type);

        // Insert a provisional signature BEFORE checking the body so recursive
        // self-calls type (as Unknown return) instead of "not defined".
        self.env.insert(
            name.to_string(),
            Type::Function {
                params: param_types.clone(),
                ret: Box::new(ret_ann.clone().unwrap_or(Type::Unknown)),
            },
        );

        // Bind params, snapshot/restore like the interpreter's call_function.
        let saved: Vec<(String, Option<Type>)> = params
            .iter()
            .map(|(n, _)| (n.clone(), self.env.get(n).cloned()))
            .collect();
        for ((n, _), t) in params.iter().zip(param_types.iter()) {
            self.env.insert(n.clone(), t.clone());
        }
        let body_result = self.synth(body);
        for (n, old) in saved {
            match old {
                Some(t) => {
                    self.env.insert(n, t);
                }
                None => {
                    self.env.remove(&n);
                }
            }
        }
        let body_t = body_result?;

        if let Some(rt) = &ret_ann {
            if !compatible(&body_t, rt) {
                return Err(HelixError::new(
                    format!(
                        "function `{}` is declared to return {}, but its body produces {}",
                        name, rt, body_t
                    ),
                    line,
                    col,
                )
                .hint("make the body match the `->` return type, or drop the annotation."));
            }
        }

        // Store the final signature (inferred return if not annotated).
        let final_ret = ret_ann.unwrap_or(body_t);
        self.env.insert(
            name.to_string(),
            Type::Function {
                params: param_types,
                ret: Box::new(final_ret),
            },
        );
        Ok(())
    }

    fn synth(&mut self, e: &Expr) -> Result<Type, HelixError> {
        match e {
            Expr::Int(_) => Ok(Type::Int),
            Expr::Float(_) => Ok(Type::Float),
            Expr::Str(_) => Ok(Type::String),
            Expr::Bool(_) => Ok(Type::Bool),
            Expr::Missing => Ok(Type::Missing),
            Expr::Interp(parts) => {
                // Type-check every embedded expression (so `"{undefined}"` errors),
                // then the whole thing is a String.
                for part in parts {
                    if let crate::ast::InterpPart::Expr(e) = part {
                        self.synth(e)?;
                    }
                }
                Ok(Type::String)
            }
            Expr::Ident { name, line, col } => match self.env.get(name) {
                Some(t) => Ok(t.clone()),
                None => {
                    let names: Vec<&str> = self.env.keys().map(|s| s.as_str()).collect();
                    let mut err =
                        HelixError::new(format!("`{}` is not defined", name), *line, *col);
                    if let Some(s) = suggest(name, &names) {
                        err = err.hint(format!("did you mean `{}`?", s));
                    } else {
                        err = err.hint(format!("assign it first, e.g. `{} = ...`.", name));
                    }
                    Err(err)
                }
            },
            Expr::Array(items) => {
                let mut t = Type::Missing; // identity for join (drops out)
                for it in items {
                    let et = self.synth(it)?;
                    t = if items.len() == 1 { et } else { join(&t, &et) };
                }
                if items.is_empty() {
                    Ok(array_of_unknown())
                } else {
                    Ok(Type::Array(Box::new(t)))
                }
            }
            Expr::Tuple(items) => {
                let mut tys = Vec::with_capacity(items.len());
                for it in items {
                    tys.push(self.synth(it)?);
                }
                Ok(Type::Tuple(tys))
            }
            Expr::Record(fields) => {
                let mut tys = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    tys.push((k.clone(), self.synth(v)?));
                }
                Ok(Type::Record(tys))
            }
            Expr::Field {
                recv,
                name,
                line,
                col,
            } => {
                let rt = self.synth(recv)?;
                match &rt {
                    Type::Record(fields) => fields
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| {
                            let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
                            let mut err = HelixError::new(
                                format!("record has no field `{}`", name),
                                *line,
                                *col,
                            );
                            if let Some(s) = suggest(name, &keys) {
                                err = err.hint(format!("did you mean `{}`?", s));
                            } else {
                                err = err.hint(format!("fields: {}", keys.join(", ")));
                            }
                            err
                        }),
                    Type::Unknown | Type::Missing => Ok(Type::Unknown),
                    other => Err(field_on_non_record(other, name, *line, *col)),
                }
            }
            Expr::Unary {
                op, expr, line, col,
            } => self.synth_unary(op, expr, *line, *col),
            Expr::Binary {
                op,
                left,
                right,
                line,
                col,
            } => {
                let lt = self.synth(left)?;
                let rt = self.synth(right)?;
                self.synth_binary(op, &lt, &rt, *line, *col)
            }
            Expr::Call {
                name,
                args,
                line,
                col,
            } => {
                let mut arg_types = Vec::with_capacity(args.len());
                for a in args {
                    arg_types.push(self.synth(a)?);
                }
                self.synth_call(name, &arg_types, *line, *col)
            }
            Expr::Method {
                recv,
                name,
                args,
                line,
                col,
            } => self.synth_method(recv, name, args, *line, *col),
            Expr::Index {
                recv,
                index,
                line,
                col,
            } => {
                let rt = self.synth(recv)?;
                let it = self.synth(index)?;
                // index must be an integer (Unknown/Missing pass)
                if !compatible(&it, &Type::Int) {
                    return Err(type_err("index", "an integer", &it, *line, *col));
                }
                Ok(match rt {
                    Type::Array(el) => *el,
                    // index is dynamic, so a tuple element is the join of all
                    // element types (precise when homogeneous, e.g. `(Int, Int)`).
                    Type::Tuple(els) => els.iter().fold(Type::Missing, |a, t| join(&a, t)),
                    Type::String | Type::Dna => Type::String,
                    Type::Unknown | Type::Missing | Type::Tensor => Type::Unknown,
                    other => {
                        return Err(HelixError::new(
                            format!("a value of type {} cannot be indexed", other),
                            *line,
                            *col,
                        ))
                    }
                })
            }
            Expr::Slice {
                recv,
                start,
                stop,
                step,
                line,
                col,
            } => {
                let rt = self.synth(recv)?;
                // each present bound must be an integer (Unknown/Missing pass)
                for bound in [start, stop, step].into_iter().flatten() {
                    let bt = self.synth(bound)?;
                    if !compatible(&bt, &Type::Int) {
                        return Err(type_err("slice bound", "an integer", &bt, *line, *col));
                    }
                }
                // slicing preserves the collection type
                Ok(match rt {
                    Type::Array(_) | Type::String | Type::Dna => rt,
                    Type::Unknown | Type::Missing | Type::Tensor => Type::Unknown,
                    other => {
                        return Err(HelixError::new(
                            format!("a value of type {} cannot be sliced", other),
                            *line,
                            *col,
                        )
                        .hint("slicing works on arrays, strings, and DNA sequences."))
                    }
                })
            }
            Expr::Lambda { params, body } => {
                // Standalone lambda: params default to Unknown.
                let saved: Vec<(String, Option<Type>)> = params
                    .iter()
                    .map(|n| (n.clone(), self.env.get(n).cloned()))
                    .collect();
                for n in params {
                    self.env.insert(n.clone(), Type::Unknown);
                }
                let body_result = self.synth(body);
                for (n, old) in saved {
                    match old {
                        Some(t) => {
                            self.env.insert(n, t);
                        }
                        None => {
                            self.env.remove(&n);
                        }
                    }
                }
                let body_t = body_result?;
                Ok(Type::Function {
                    params: params.iter().map(|_| Type::Unknown).collect(),
                    ret: Box::new(body_t),
                })
            }
            Expr::Let { bindings, body } => {
                let mut saved: Vec<(String, Option<Type>)> = Vec::with_capacity(bindings.len());
                for (name, expr) in bindings {
                    let t = self.synth(expr)?;
                    let prev = self.env.insert(name.clone(), t);
                    saved.push((name.clone(), prev));
                }
                let result = self.synth(body);
                for (name, prev) in saved.into_iter().rev() {
                    match prev {
                        Some(t) => {
                            self.env.insert(name, t);
                        }
                        None => {
                            self.env.remove(&name);
                        }
                    }
                }
                result
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                line,
                col,
            } => {
                let ct = self.synth(cond)?;
                if !matches!(ct, Type::Bool | Type::Missing | Type::Unknown) {
                    return Err(HelixError::new(
                        format!("`if` condition must be a boolean, found a value of type {}", ct),
                        *line,
                        *col,
                    )
                    .hint("use an explicit comparison, e.g. `if x > 0 then ... else ...`."));
                }
                let tt = self.synth(then_branch)?;
                let et = self.synth(else_branch)?;
                Ok(join(&tt, &et))
            }
        }
    }

}


mod signatures;
use signatures::*;

/// Type-check a whole program. Runs after parse, before interpretation.
pub fn check(program: &[Stmt]) -> Result<TypeMap, HelixError> {
    let mut checker = Checker::new();
    for s in program {
        checker.exec_stmt(s)?;
    }
    Ok(checker.types)
}


#[cfg(test)]
mod tests;

mod synth;
