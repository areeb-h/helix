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

    fn synth_unary(
        &mut self,
        op: &UnOp,
        expr: &Expr,
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        let t = self.synth(expr)?;
        match op {
            UnOp::Neg => match &t {
                Type::Int | Type::Float | Type::Num | Type::Missing | Type::Unknown => Ok(t),
                Type::Tensor | Type::Array(_) => Ok(Type::Unknown),
                other => Err(HelixError::new(
                    format!("cannot negate a value of type {}", other),
                    line,
                    col,
                )),
            },
            UnOp::Not => match &t {
                Type::Bool | Type::Missing | Type::Unknown => Ok(t),
                Type::Array(_) | Type::Tensor => Ok(Type::Unknown),
                other => Err(HelixError::new(
                    format!("expected a boolean, found a value of type {}", other),
                    line,
                    col,
                )),
            },
        }
    }

    fn synth_binary(
        &mut self,
        op: &BinOp,
        lt: &Type,
        rt: &Type,
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        use BinOp::*;
        match op {
            // `a ?? b` — result is whichever side survives; never errors.
            Coalesce => Ok(join(lt, rt)),
            // Equality works on any two operands and never errors.
            Eq | Ne => Ok(Type::Bool),
            And | Or => {
                for t in [lt, rt] {
                    if !matches!(t, Type::Bool | Type::Missing | Type::Unknown) {
                        return Err(HelixError::new(
                            format!("expected a boolean, found a value of type {}", t),
                            line,
                            col,
                        )
                        .hint("Helix has no \"truthiness\" — use an explicit comparison like `x > 0`."));
                    }
                }
                Ok(Type::Bool)
            }
            Lt | Gt | Le | Ge => {
                if matches!(lt, Type::Unknown | Type::Missing)
                    || matches!(rt, Type::Unknown | Type::Missing)
                {
                    return Ok(Type::Bool);
                }
                let both_str = matches!(lt, Type::String) && matches!(rt, Type::String);
                if both_str || (is_numeric(lt) && is_numeric(rt)) {
                    Ok(Type::Bool)
                } else {
                    Err(HelixError::new(
                        format!(
                            "operator `{}` needs numbers, but got a {}",
                            op.symbol(),
                            if is_numeric(lt) { rt } else { lt }
                        ),
                        line,
                        col,
                    ))
                }
            }
            // Arithmetic
            _ => {
                if matches!(lt, Type::Unknown) || matches!(rt, Type::Unknown) {
                    return Ok(Type::Unknown);
                }
                if matches!(lt, Type::Missing) || matches!(rt, Type::Missing) {
                    return Ok(Type::Missing);
                }
                match arith_broadcast(op, lt, rt) {
                    Some(t) => Ok(t),
                    None => Err(HelixError::new(
                        format!(
                            "operator `{}` needs numbers, but got a {}",
                            op.symbol(),
                            if is_numeric(lt) || matches!(lt, Type::Array(_) | Type::Tensor) {
                                rt
                            } else {
                                lt
                            }
                        ),
                        line,
                        col,
                    )),
                }
            }
        }
    }

    fn synth_call(
        &mut self,
        name: &str,
        args: &[Type],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        if BUILTIN_FNS.contains(&name) {
            return builtin_type(name, args, line, col);
        }
        // user-defined function?
        if let Some(Type::Function { params, ret }) = self.env.get(name).cloned() {
            if params.len() != args.len() {
                return Err(HelixError::new(
                    format!(
                        "`{}` expects {} argument{}, got {}",
                        name,
                        params.len(),
                        if params.len() == 1 { "" } else { "s" },
                        args.len()
                    ),
                    line,
                    col,
                ));
            }
            for (i, (p, a)) in params.iter().zip(args.iter()).enumerate() {
                if !compatible(a, p) {
                    return Err(HelixError::new(
                        format!(
                            "argument {} of `{}` should be {}, found a value of type {}",
                            i + 1,
                            name,
                            p,
                            a
                        ),
                        line,
                        col,
                    ));
                }
            }
            return Ok(*ret);
        }
        if let Some(t) = self.env.get(name) {
            return Err(HelixError::new(
                format!("`{}` is a {}, not a function", name, t),
                line,
                col,
            )
            .hint("only functions and the built-ins can be called."));
        }
        // unknown — suggest from builtins + user functions
        let mut cands: Vec<String> = BUILTIN_FNS.iter().map(|s| s.to_string()).collect();
        cands.extend(
            self.env
                .iter()
                .filter(|(_, t)| matches!(t, Type::Function { .. }))
                .map(|(k, _)| k.clone()),
        );
        let cand_refs: Vec<&str> = cands.iter().map(|s| s.as_str()).collect();
        let mut err = HelixError::new(format!("`{}` is not a known function", name), line, col);
        if let Some(s) = suggest(name, &cand_refs) {
            err = err.hint(format!("did you mean `{}`?", s));
        }
        Err(err)
    }

    fn synth_method(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        let rt = self.synth(recv)?;
        // Record the receiver's type so the bytecode compiler can route this method
        // by the receiver's true type (DataFrame vs Array vs Tensor), not its name.
        self.types.insert(recv as *const Expr, rt.clone());
        // `.is_missing()` is universal.
        if name == "is_missing" {
            return Ok(Type::Bool);
        }
        match &rt {
            // Permissive: any method on Unknown/Missing receiver is Unknown.
            Type::Unknown | Type::Missing => Ok(Type::Unknown),
            // DataFrame / GroupBy: args are the runtime schema boundary — UNCHECKED.
            Type::DataFrame => df_method_type(name, line, col),
            Type::GroupBy => groupby_method_type(name, line, col),
            Type::Array(el) => self.synth_array_method(el, name, args, line, col),
            Type::Tensor => {
                self.synth_simple_args(args)?;
                tensor_method_type(name, args.len(), line, col)
            }
            Type::String => {
                self.synth_simple_args(args)?;
                string_method_type(name, line, col)
            }
            Type::Dna => {
                self.synth_simple_args(args)?;
                dna_method_type(name, line, col)
            }
            other => Err(HelixError::new(
                format!("type {} has no method `{}`", other, name),
                line,
                col,
            )),
        }
    }

    /// Evaluate argument types for type-checking side effects (so e.g.
    /// `xs.take(undefined)` reports the undefined name).
    fn synth_simple_args(&mut self, args: &[Expr]) -> Result<(), HelixError> {
        for a in args {
            self.synth(a)?;
        }
        Ok(())
    }

    fn synth_array_method(
        &mut self,
        el: &Type,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Type, HelixError> {
        // Comprehension methods take UNEVALUATED bodies with `it` / lambda binders.
        match name {
            "map" | "filter" | "where" | "any" | "all" => {
                let (params, body) = comprehension_params(args);
                let body_t = self.with_pattern(&params, el.clone(), body)?;
                match name {
                    "map" => Ok(Type::Array(Box::new(body_t))),
                    "filter" | "where" => {
                        require_boolish(&body_t, name, line, col)?;
                        Ok(Type::Array(Box::new(el.clone())))
                    }
                    _ => {
                        require_boolish(&body_t, name, line, col)?;
                        Ok(Type::Bool)
                    }
                }
            }
            "reduce" => {
                if args.len() != 2 {
                    return Ok(Type::Unknown); // malformed → runtime errors; don't false-positive
                }
                let init_t = self.synth(&args[0])?;
                if let Expr::Lambda { params, body } = &args[1] {
                    if params.len() == 2 {
                        let body_t = self.with_two(
                            &params[0],
                            init_t.clone(),
                            &params[1],
                            el.clone(),
                            body,
                        )?;
                        return Ok(join(&init_t, &body_t));
                    }
                }
                Ok(Type::Unknown)
            }
            _ => {
                self.synth_simple_args(args)?;
                array_method_type(name, el, line, col)
            }
        }
    }

    /// Synth `body` with `name` bound to `t`, restoring afterward (nested
    /// comprehensions restore correctly).
    fn with_binder(&mut self, name: &str, t: Type, body: &Expr) -> Result<Type, HelixError> {
        let saved = self.env.insert(name.to_string(), t);
        let result = self.synth(body);
        match saved {
            Some(old) => {
                self.env.insert(name.to_string(), old);
            }
            None => {
                self.env.remove(name);
            }
        }
        result
    }

    /// Bind one element type to one name, or destructure it across several
    /// names (`(a, b) => ...`), then synthesize the body type.
    fn with_pattern(&mut self, names: &[String], el: Type, body: &Expr) -> Result<Type, HelixError> {
        if names.len() == 1 {
            return self.with_binder(&names[0], el, body);
        }
        // element types for each destructured name
        let types: Vec<Type> = match &el {
            Type::Tuple(ts) if ts.len() == names.len() => ts.clone(),
            Type::Array(inner) => vec![(**inner).clone(); names.len()],
            _ => vec![Type::Unknown; names.len()], // mismatch/Unknown -> permissive
        };
        let saved: Vec<(String, Option<Type>)> = names
            .iter()
            .map(|n| (n.clone(), self.env.get(n).cloned()))
            .collect();
        for (n, t) in names.iter().zip(types.into_iter()) {
            self.env.insert(n.clone(), t);
        }
        let result = self.synth(body);
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
        result
    }

    fn with_two(
        &mut self,
        n1: &str,
        t1: Type,
        n2: &str,
        t2: Type,
        body: &Expr,
    ) -> Result<Type, HelixError> {
        let s1 = self.env.insert(n1.to_string(), t1);
        let s2 = self.env.insert(n2.to_string(), t2);
        let result = self.synth(body);
        match s2 {
            Some(old) => {
                self.env.insert(n2.to_string(), old);
            }
            None => {
                self.env.remove(n2);
            }
        }
        match s1 {
            Some(old) => {
                self.env.insert(n1.to_string(), old);
            }
            None => {
                self.env.remove(n1);
            }
        }
        result
    }
}

// `it` by default, or the single lambda param. Body is the (sole) arg expr.
fn comprehension_params(args: &[Expr]) -> (Vec<String>, &Expr) {
    match args.first() {
        Some(Expr::Lambda { params, body }) => (params.clone(), body),
        Some(e) => (vec!["it".to_string()], e),
        None => (vec!["it".to_string()], &Expr::Missing),
    }
}

fn require_boolish(t: &Type, name: &str, line: usize, col: usize) -> Result<(), HelixError> {
    if matches!(t, Type::Bool | Type::Missing | Type::Unknown) {
        Ok(())
    } else {
        Err(HelixError::new(
            format!(
                "`{}` expects a yes/no test, but the expression produces a value of type {}",
                name, t
            ),
            line,
            col,
        )
        .hint("write a comparison, e.g. `xs.filter(it > 50)`."))
    }
}

// ---------- signature tables ----------

fn builtin_type(name: &str, args: &[Type], line: usize, col: usize) -> Result<Type, HelixError> {
    let any = |ts: &[Type], f: fn(&Type) -> bool| ts.iter().any(f);
    // math: container/Unknown ⇒ Unknown; Missing ⇒ Missing (the false-positive guard)
    if MATH_UNARY_FLOAT.contains(&name)
        || matches!(name, "floor" | "ceil" | "round" | "trunc" | "abs" | "sign")
    {
        if args.len() != 1 {
            return Err(arity_err(name, 1, args.len(), line, col));
        }
        let a = &args[0];
        if matches!(a, Type::Array(_) | Type::Tensor | Type::Unknown) {
            return Ok(Type::Unknown);
        }
        if matches!(a, Type::Missing) {
            return Ok(Type::Missing);
        }
        if !is_numeric(a) {
            return Err(type_err(name, "a number or array of numbers", a, line, col));
        }
        return Ok(match name {
            "floor" | "ceil" | "round" | "trunc" | "sign" => Type::Int,
            "abs" => a.clone(),
            _ => Type::Float,
        });
    }
    match name {
        "print" => Ok(Type::Unit),
        "dna" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err("dna", "a string", &args[0], line, col));
            }
            Ok(Type::Dna)
        }
        "range" => {
            if args.is_empty() || args.len() > 2 {
                return Err(HelixError::new(
                    format!("`range` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                ));
            }
            for a in args {
                if !compatible(a, &Type::Int) {
                    return Err(type_err("range", "an integer", a, line, col));
                }
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "read_csv" | "read_parquet" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            Ok(Type::DataFrame)
        }
        "read_fasta" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::String) {
                return Err(type_err(name, "a string path", &args[0], line, col));
            }
            // An array of `{id, seq, length}` records; element kept `Unknown` so
            // field/sequence-method access stays permissive.
            Ok(Type::Array(Box::new(Type::Unknown)))
        }
        "write_parquet" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::DataFrame) {
                return Err(type_err("write_parquet", "a DataFrame", &args[0], line, col));
            }
            if !compatible(&args[1], &Type::String) {
                return Err(type_err("write_parquet", "a string path", &args[1], line, col));
            }
            Ok(Type::Unit)
        }
        "tensor" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            let a = &args[0];
            if is_numeric(a)
                || matches!(a, Type::Array(_) | Type::Unknown | Type::Missing)
            {
                Ok(Type::Tensor)
            } else {
                Err(type_err("tensor", "a number or array", a, line, col))
            }
        }
        "zeros" | "ones" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &array_of_unknown()) {
                return Err(type_err(name, "an array like `[2, 3]`", &args[0], line, col));
            }
            Ok(Type::Tensor)
        }
        "eye" => {
            if args.len() != 1 {
                return Err(arity_err(name, 1, args.len(), line, col));
            }
            if !compatible(&args[0], &Type::Int) {
                return Err(type_err("eye", "an integer", &args[0], line, col));
            }
            Ok(Type::Tensor)
        }
        // two-arg math
        "log" | "atan2" | "hypot" | "min" | "max" => {
            if args.len() != 2 {
                return Err(arity_err(name, 2, args.len(), line, col));
            }
            if any(args, |t| matches!(t, Type::Unknown)) {
                return Ok(Type::Unknown);
            }
            if any(args, |t| matches!(t, Type::Missing)) {
                return Ok(Type::Missing);
            }
            for a in args {
                if !is_numeric(a) {
                    return Err(type_err(name, "a number", a, line, col));
                }
            }
            Ok(if matches!(name, "min" | "max") {
                Type::Num
            } else {
                Type::Float
            })
        }
        _ => Ok(Type::Unknown), // unreachable (BUILTIN_FNS gated), but stay permissive
    }
}

fn array_method_type(name: &str, el: &Type, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "mean" | "std" => Type::Float,
        "sum" => Type::Num,
        "min" | "max" | "first" | "last" => el.clone(),
        "count" => Type::Int,
        "normalize" => Type::Array(Box::new(Type::Float)),
        "sort" | "reverse" | "drop_missing" | "take" | "drop" => Type::Array(Box::new(el.clone())),
        // `enumerate` -> Array of (Int, element) tuples; `zip` -> Array of pairs
        // (the other side's element type isn't threaded, so its 2nd slot is Unknown).
        "enumerate" => Type::Array(Box::new(Type::Tuple(vec![Type::Int, el.clone()]))),
        "zip" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Unknown]))),
        // `(value, count)` tuples for the n most frequent elements.
        "top" => Type::Array(Box::new(Type::Tuple(vec![el.clone(), Type::Int]))),
        _ => return Err(unknown_method("Array", name, ARRAY_METHODS, line, col)),
    })
}

fn string_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "upper" | "lower" | "reverse" => Type::String,
        "count" => Type::Int,
        _ => return Err(unknown_method("String", name, STRING_METHODS, line, col)),
    })
}

fn dna_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "length" => Type::Int,
        "gc_content" => Type::Float,
        "complement" | "reverse_complement" => Type::Dna,
        "kmers" => Type::Array(Box::new(Type::String)),
        // 0-based index of the motif, or `missing` when absent.
        "find" => Type::Int,
        _ => return Err(unknown_method("Dna", name, DNA_METHODS, line, col)),
    })
}

fn tensor_method_type(name: &str, nargs: usize, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "shape" => Type::Array(Box::new(Type::Int)),
        "ndim" | "count" => Type::Int,
        // sum/mean/min/max: 0 args → scalar Float; 1 axis arg → Tensor.
        "sum" | "mean" | "min" | "max" => {
            if nargs == 0 {
                Type::Float
            } else {
                Type::Tensor
            }
        }
        "flatten" | "reshape" | "transpose" | "t" | "inv" | "solve" => Type::Tensor,
        "matmul" | "dot" => Type::Unknown, // Float for vec·vec, Tensor otherwise
        "norm" | "det" => Type::Float,
        _ => return Err(unknown_method("Tensor", name, TENSOR_METHODS, line, col)),
    })
}

fn df_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "where" | "filter" | "select" | "sort" | "head" | "cache" => Type::DataFrame,
        "group" => Type::GroupBy,
        "count" => Type::Int,
        "columns" => Type::Array(Box::new(Type::String)),
        _ => return Err(unknown_method("DataFrame", name, DF_METHODS, line, col)),
    })
}

fn groupby_method_type(name: &str, line: usize, col: usize) -> Result<Type, HelixError> {
    Ok(match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => Type::DataFrame,
        _ => return Err(unknown_method("GroupBy", name, GROUPBY_AGGS, line, col)),
    })
}

/// Type-check a whole program. Runs after parse, before interpretation.
pub fn check(program: &[Stmt]) -> Result<TypeMap, HelixError> {
    let mut checker = Checker::new();
    for s in program {
        checker.exec_stmt(s)?;
    }
    Ok(checker.types)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(src: &str) -> Result<(), HelixError> {
        let toks = crate::lexer::lex(src)?;
        let prog = crate::parser::parse(toks)?;
        check(&prog).map(|_| ())
    }
    fn ok(src: &str) {
        let r = tc(src);
        assert!(r.is_ok(), "expected OK for `{}`, got: {:?}", src, r.err().map(|e| e.message));
    }
    fn emsg(src: &str) -> String {
        tc(src).expect_err("expected a type error").message
    }

    #[test]
    fn well_typed_programs_pass() {
        ok("x = 5\ny = x + 1");
        ok("[1, 2, 3].map(it + 1).sum()");
        ok("grade = if 5 > 3 then \"A\" else \"B\"");
        ok("missing + 1");
        ok("[1, missing, 3].mean()");
        ok("x = 5\nx.is_missing()");
        ok("seq = dna(\"ATGC\")\nseq.gc_content()");
        ok("tensor([[1, 2], [3, 4]]).matmul(tensor([[1, 0], [1, 1]])).sum()");
        ok("tensor([3, 4]).norm()");
        ok("scores = [1, 2, 3]\nscores.reduce(0, (a, x) => a + x)");
        ok("grid = [[1, 2], [3, 4]]\ngrid.map(row => row.map(v => v + 1))");
        // recursion (un-annotated params -> Unknown, never false-positive)
        ok("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(5)");
        ok("[1, \"a\", true].count()"); // mixed array -> Array(Unknown), fine
    }

    #[test]
    fn annotations_check() {
        ok("fn area(w: Int, h: Int) -> Int = w * h\narea(3, 4)");
        ok("fn f(x: Int) = x + 1\nf(2)");
        ok("fn norm2(xs) -> Float = sqrt((xs * xs).sum())"); // body Unknown compat any ret
    }

    #[test]
    fn dataframe_columns_unchecked() {
        // column names are the runtime schema boundary — never type-checked
        ok("read_csv(\"x.csv\").where(age > 40 and hr < 75).select(name, age).sort(age).count()");
        ok("read_csv(\"g.csv\").group(species).mean(expression).columns()");
    }

    #[test]
    fn catches_provable_errors() {
        assert!(emsg("5 + \"x\"").contains("needs numbers"));
        assert!(emsg("if 5 then 1 else 2").contains("must be a boolean"));
        assert!(emsg("velociti(3)").contains("not a known function"));
        assert!(emsg("[1, 2].maen()").contains("no method"));
        assert!(emsg("5 and true").contains("boolean"));
        assert!(emsg("range(1, 2, 3)").contains("1 or 2"));
        assert!(emsg("fn f(x: Int) -> String = x + 1").contains("declared to return"));
        assert!(emsg("xs = [1, 2]\nxs[\"a\"]").contains("integer"));
        assert!(emsg("undefinedvar").contains("not defined"));
        assert!(emsg("dna(5)").contains("expected a string"));
    }

    #[test]
    fn let_in_typecheck() {
        ok("let x = 5 in x + 1");
        ok("let a = 1, b = a + 1 in a + b"); // sequential
        ok("fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m) ** 2).sum() / n");
        // a type error inside the body is caught
        assert!(emsg("let x = \"a\" in x + 1").contains("needs numbers"));
        // the let binding's scope doesn't leak: `y` is undefined outside
        assert!(emsg("z = let y = 1 in y\ny").contains("not defined"));
    }

    #[test]
    fn tuples_and_destructuring_typecheck() {
        ok("p = (3, 4)\np[0] + p[1]"); // homogeneous tuple index -> Int
        ok("a, b = (1, 2)\na + b");
        ok("x, y, z = [1, 2, 3]\nx + y + z"); // array destructure
        ok("fn pair(n) = (n, n + 1)\nlo, hi = pair(5)\nlo + hi");
        ok("[1, 2].zip([3, 4]).map(it[0] + it[1])");
        ok("[7, 8].enumerate().map(it[0])");
        // lambda-param destructuring (the nicer form)
        ok("[(1, 2), (3, 4)].map((a, b) => a + b)");
        ok("[1, 2].zip([3, 4]).map((a, b) => a + b)");
        ok("[7, 8].enumerate().where((i, v) => v > 0).map((i, v) => i)");
        // length mismatch is caught at compile time (tuple has a known arity)
        assert!(emsg("a, b = (1, 2, 3)").contains("cannot destructure"));
        // destructuring a scalar is a compile error
        assert!(emsg("a, b = 5").contains("cannot destructure"));
    }

    #[test]
    fn slicing_typecheck() {
        ok("xs = [1, 2, 3, 4]\nxs[1:3].sum()"); // slice of array stays an array
        ok("\"hello\"[::-1].upper()"); // slice of string stays a string
        ok("xs = [1, 2, 3]\nxs[:]"); // bare slice
        // a non-integer bound is a compile error
        assert!(emsg("xs = [1, 2, 3]\nxs[\"a\":]").contains("integer"));
        // slicing a non-sliceable type errors
        assert!(emsg("(5)[1:2]").contains("cannot be sliced"));
    }

    #[test]
    fn records_typecheck() {
        ok("r = {name: \"Ada\", age: 41}\nr.age + 1");
        ok("fn stats(xs) = {mean: xs.mean(), n: xs.count()}\nstats([1, 2, 3]).mean");
        ok("[{age: 10}, {age: 20}].map(it.age).mean()");
        // field typo caught at compile time, with a suggestion
        assert!(emsg("r = {name: \"A\", age: 1}\nr.naem").contains("no field"));
        assert_eq!(
            tc("r = {name: \"A\"}\nr.naem").unwrap_err().hint.as_deref(),
            Some("did you mean `name`?")
        );
        // method without parens → helpful "call it with ()" hint
        assert!(tc("[1, 2].mean")
            .unwrap_err()
            .hint
            .as_deref()
            .unwrap()
            .contains("call it with `mean()`"));
    }

    #[test]
    fn interpolation_and_coalesce() {
        ok("name = \"x\"\nprint(\"hi {name} {1 + 2}\")");
        ok("x = missing\nprint(\"v = {x ?? 0}\")");
        ok("config = missing\ntimeout = config ?? 30");
        // embedded expressions are type-checked: undefined names error
        assert!(emsg("print(\"hi {nope}\")").contains("not defined"));
        // ?? never errors (any operands)
        ok("\"a\" ?? 1");
    }

    #[test]
    fn suggests_on_typos() {
        assert_eq!(
            tc("[1, 2].maen()").unwrap_err().hint.as_deref(),
            Some("did you mean `mean`?")
        );
    }

    #[test]
    fn unknown_type_annotation_errors() {
        assert!(emsg("fn g(x: Intt) = x").contains("unknown type"));
    }

    #[test]
    fn examples_have_zero_false_positives() {
        // THE hard guarantee: every shipped example must type-check clean.
        for name in [
            "tour",
            "functions",
            "analysis",
            "math",
            "dataframes",
            "tensors",
            "errors",
            "typed",
            "strings",
            "records",
            "slicing",
            "tuples",
            "bindings",
            "genomics",
        ] {
            let src = std::fs::read_to_string(format!("examples/{}.helix", name))
                .unwrap_or_else(|_| panic!("read examples/{}.helix", name));
            let toks = crate::lexer::lex(&src).expect("lex");
            let prog = crate::parser::parse(toks).expect("parse");
            let r = check(&prog);
            assert!(
                r.is_ok(),
                "example `{}` must type-check clean, got: {:?}",
                name,
                r.err().map(|e| e.message)
            );
        }
    }
}
