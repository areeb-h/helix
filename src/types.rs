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

use rustc_hash::{FxHashMap, FxHashSet};

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
        /// How many of `params` a call must supply; the rest have defaults. A call is
        /// refused outside `required..=params.len()`, in the runtime's words.
        required: usize,
        /// The top-level `fn` this is the signature of, or `None` for a lambda or any other
        /// function value. Call-site specialization re-types a BODY, so it must know the
        /// call resolved to the function that owns it: a `let g = …` lambda shadowing a
        /// top-level `fn g` was specialized with `g`'s body, by name, and refused a program
        /// that ran. Identity travels with the type; nothing else reads it.
        origin: Option<std::rc::Rc<str>>,
    },
    /// The OPEN kinds an annotation names (field build, 1.45a): a record, dict, tuple or
    /// function whose shape is not known statically. Compatible with every value of that
    /// kind and with nothing else, so a wrong KIND is refused at the call; inside a body the
    /// value's fields, elements and methods answer `Unknown`, as an unannotated
    /// parameter's do. `Dict` has no structural twin: a dict literal is `Unknown` to the
    /// checker (the opaque-type pattern), so the annotation is the only place it appears.
    AnyRecord,
    Dict,
    AnyTuple,
    AnyFunction,
    Unit,
    /// Absent data. BOTTOM: compatible with everything; drops under `join`.
    Missing,
    /// Permissive TOP (Any/Dynamic): compatible with everything; NEVER errors.
    Unknown,
    /// No value at all — the type of `raise(…)`. BOTTOM for control flow: compatible with
    /// everything (what follows never runs) and dropped under `join`, so `if bad then
    /// raise("…") else {rec}` has the record's shape and `x ?? raise("…")` has `x`'s (field
    /// build, 1.44a: a constructor that validated inline widened to Unknown, and the
    /// specialization answered nothing). Where `Missing` is a VALUE that is absent, `Never`
    /// is the absence of a value; everywhere but `join` it is read as `Unknown` is, so
    /// nothing that ran is refused for standing after a raise.
    Never,
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
            Type::AnyRecord => write!(f, "Record"),
            Type::Dict => write!(f, "Dict"),
            Type::AnyTuple => write!(f, "Tuple"),
            Type::AnyFunction => write!(f, "Function"),
            Type::Unit => write!(f, "Unit"),
            Type::Missing => write!(f, "Missing"),
            Type::Never => write!(f, "Never"),
            Type::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Probe the checker's own `builtin_type` table with synthetic argument types — the seam
/// `helix describe` derives arity and return types through. One source of truth: the same
/// match arms that type-check real programs answer the catalog, so the catalog cannot
/// drift from the language (the argument `describe` already makes for names, extended to
/// signatures). Sound because `compatible(Unknown, _)` is true and the non-arity guards
/// admit `Unknown`, so an `Err` here is an ARITY rejection, not a type one. Line/col are
/// zeroed: the probe never renders an error.
pub(crate) fn probe_builtin(name: &str, args: &[Type]) -> Option<Type> {
    signatures::builtin_type(name, args, 0, 0).ok()
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
        TypeAnn::Record => Type::AnyRecord,
        TypeAnn::Dict => Type::Dict,
        TypeAnn::Tuple => Type::AnyTuple,
        TypeAnn::Function => Type::AnyFunction,
        TypeAnn::Any => Type::Unknown,
    }
}

/// Whether a value of type `actual` may bind to a parameter (or a return) ANNOTATED `ann`.
/// [`compatible`], with one edge: an `Int` annotation refuses a `Float`. The numeric tower
/// makes Int and Float compatible so an unannotated program never rejects — but an annotation
/// is the author saying which one, and `type_of(1.5)` is `"Float"`, so static and dynamic
/// must agree (field build, 1.45c). A `Float` annotation still admits an Int, as arithmetic
/// promotes one; `Num` admits both, as it says.
pub fn annotation_admits(ann: &Type, actual: &Type) -> bool {
    !matches!((ann, actual), (Type::Int, Type::Float)) && compatible(ann, actual)
}

/// The ONLY source of type errors. Symmetric. `Unknown`/`Missing` are compatible
/// with everything; numerics form one tower; `Array`/`Function` are structural.
pub fn compatible(a: &Type, b: &Type) -> bool {
    use Type::*;
    match (a, b) {
        (Unknown, _) | (_, Unknown) => true,
        (Missing, _) | (_, Missing) => true,
        (Never, _) | (_, Never) => true,
        _ if a == b => true,
        _ if is_numeric(a) && is_numeric(b) => true,
        (Array(x), Array(y)) => compatible(x, y),
        (Tuple(a), Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| compatible(x, y))
        }
        (Record(_), Record(_)) => true, // permissive; field access does the checking
        // The open kinds: any value of the kind, and nothing else.
        (AnyRecord, Record(_)) | (Record(_), AnyRecord) => true,
        (AnyTuple, Tuple(_)) | (Tuple(_), AnyTuple) => true,
        (AnyFunction, Function { .. }) | (Function { .. }, AnyFunction) => true,

        (
            Function { params: p1, ret: r1, .. },
            Function { params: p2, ret: r2, .. },
        ) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2.iter()).all(|(x, y)| compatible(x, y))
                && compatible(r1, r2)
        }
        _ => false,
    }
}

/// Least-upper-bound. TOTAL — never errors; incompatible pairs widen to
/// `Unknown`. Used for `if` branches and array elements so they never reject. `Never` (a
/// branch that raises) contributes no value and drops first; `Missing` drops next.
pub fn join(a: &Type, b: &Type) -> Type {
    use Type::*;
    match (a, b) {
        _ if a == b => a.clone(),
        (Never, t) | (t, Never) => t.clone(),
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

/// `try EXPR` binds tighter than any binary operator, so `try 1 + 1` parses as
/// `(try 1) + 1` — the `try` yields its `{ok, value, error}` record and `+ 1` then
/// fails on a Record. The resulting error is *true* and useless: it names a Record
/// in an expression that contains no visible record.
///
/// When an operand is literally a `try` expression, say so and name the fix. The
/// test is on the **AST node**, not on the record's shape, so a user's own
/// `{ok: …, value: …, error: …}` record never triggers this hint.
///
/// Whether `try` *should* bind looser is a separate, breaking parser question; this
/// only makes the existing behaviour explicable.
fn try_binds_tighter_hint(e: HelixError, op: &BinOp, left: &Expr, right: &Expr) -> HelixError {
    if e.hint.is_some() {
        return e; // never displace a more specific hint
    }
    let side = if matches!(left, Expr::Try { .. }) {
        Some("left")
    } else if matches!(right, Expr::Try { .. }) {
        Some("right")
    } else {
        None
    };
    match side {
        Some(_) => e.hint(format!(
            "`try` binds tighter than `{0}`, so this is `(try …) {0} …` and the `try` \
             produced a `{{ok, value, error}}` record — parenthesize the whole \
             expression: `try (a {0} b)`.",
            op.symbol()
        )),
        None => e,
    }
}

/// The arity refusal, in the one sentence every layer speaks — this is the runtime's
/// `arity_err`, so the checker cannot drift from it (it used to be a second copy, which
/// happened to agree on `takes` while the runtime said `expects`).
fn arity_err(name: &str, min: usize, max: usize, got: usize, line: usize, col: usize) -> HelixError {
    crate::interp::arity_err(name, min, max, got, line, col)
}

/// `record has no field …`, with a did-you-mean or the field list — the refusal a static
/// field read and a destructure share.
fn record_has_no_field(fields: &[(String, Type)], name: &str, line: usize, col: usize) -> HelixError {
    let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    let mut err = HelixError::new(format!("record has no field `{}`", name), line, col);
    if let Some(s) = suggest(name, &keys) {
        err = err.hint(format!("did you mean `{}`?", s));
    } else {
        // Canonical order, matching how the record itself prints.
        let mut keys = keys;
        keys.sort_unstable();
        err = err.hint(format!("fields: {}", keys.join(", ")));
    }
    err
}

/// Field access `x.name` on something that isn't a record. If `name` is actually
/// a method of that type, nudge the user to call it with `()`.
fn field_on_non_record(t: &Type, name: &str, line: usize, col: usize) -> HelixError {
    use crate::registry as reg;
    let methods: &[&str] = match t {
        Type::Array(_) => reg::ARRAY_METHODS,
        Type::String => reg::STRING_METHODS,
        Type::Dna => reg::DNA_METHODS,
        Type::Tensor => reg::TENSOR_METHODS,
        Type::DataFrame => reg::DF_METHODS,
        Type::GroupBy => reg::GROUPBY_METHODS,
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
    // ONE REFUSAL, ONE SENTENCE — the runtime's, in `interp/methods`. This said "type Int
    // has no method" while the runtime said "an Int has no method", and the two are the
    // same refusal reached by two routes: a receiver whose type is known refuses here, and
    // the same receiver through a parameter is `Unknown`, so the checker steps aside and
    // the runtime answers. A user has no way to predict which sentence they get.
    //
    // The runtime's form wins because the SIBLING family already chose it: "`f` is an Int,
    // not a function" runs through `with_article` on both sides. An article is the house
    // style for naming a value's type in an error; "type Int" was the outlier.
    //
    // The article used to double as a tell for WHICH half refused, and this cycle leaned on
    // that twice while diagnosing. That is not a reason to keep two sentences: `helix check`
    // answers the same question outright — clean means the runtime refused — and a
    // diagnostic that works by reading grammar is not one anyone can rely on.
    let err = HelixError::new(
        format!("{} has no method `{}`", crate::value::with_article(type_name), name),
        line,
        col,
    );
    match crate::suggest::hint(name, crate::suggest::Site::Method, candidates) {
        Some(h) => err.hint(h),
        // No near-miss: point at the doc command instead of dumping 79 names — a dump
        // is a haystack, `helix doc Array` is an answer. Byte-identical to the runtime
        // twin in `interp/methods.rs` so the engines cannot drift.
        None => err.hint(format!(
            "no similar method — `helix doc {type_name}` lists all {type_name} methods."
        )),
    }
}

/// `"{feat}"` with `feat` undefined produces the ordinary is-not-defined error — true,
/// and blind to WHERE the name sits: inside a string, where the braces themselves may be
/// the surprise. Fires only for a bare-`Ident` hole (a compound expression's error stands
/// on its own) and never displaces an existing hint.
fn interp_hole_hint(e: HelixError, hole: &crate::ast::Expr) -> HelixError {
    if e.hint.is_some() {
        return e;
    }
    let crate::ast::Expr::Ident { name, .. } = hole else {
        return e;
    };
    e.hint(format!(
        "`{{ }}` inside a string is interpolation — define `{name}` (or fix its \
         spelling), or escape the braces for literal text: `{{{{{name}}}}}` prints \
         `{{{name}}}`."
    ))
}

const MATH_UNARY_FLOAT: &[&str] = &[
    "sqrt", "cbrt", "exp", "ln", "log10", "log2", "sin", "cos", "tan", "asin", "acos", "atan",
    "sinh", "cosh", "tanh", "degrees", "radians", "erf", "normal_cdf", "normal_pdf",
    // neural-net activations — elementwise, so they broadcast over arrays/tensors too.
    "relu", "sigmoid",
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
    /// Names ever declared `mut` at the top level. Top-level statement flow
    /// keeps their precise types (rebinds update `env` in order), but inside a
    /// *deferred* body — a `fn` or a lambda, checked once at definition yet run
    /// at call time — a mutable global types as `Unknown`: it may hold a value
    /// of a different type by the time the body runs, so a frozen
    /// definition-time type would mis-route type-directed dispatch (e.g.
    /// compile `d.where(…)` as a DataFrame verb after `d` was rebound to an
    /// array, diverging from the walker's runtime dispatch).
    mut_globals: FxHashSet<String>,
    /// Names bound by a `fn` statement REACHED so far. Deliberately not the
    /// hoisted pre-pass: `mut f = ...` ABOVE the `fn` is legal (the definition
    /// rebinds it), while `mut f = ...` BELOW it is bind's immutable-reassign
    /// error — this set draws exactly that line.
    fn_decls: FxHashSet<String>,
    /// Names bound by a VALUE statement (`x = ...`, destructure) reached so far,
    /// plus the seeded constants — bind's actual immutable set. Deliberately NOT
    /// the whole `env`: a `fn` name is not a value global, so `fn f() = 1` then
    /// `f = 5` legally shadows the function (pinned by corpus
    /// `m1b_assign_over_fn`), while `f = 5` then `fn f() = 1` refuses.
    value_globals: FxHashSet<String>,
    /// Every name bound by a top-level value statement anywhere in the file, for
    /// DIAGNOSIS only: it lets an unbound name be reported as "not defined yet"
    /// when the file does bind it further down.
    ///
    /// A `fn` is usable above its definition (`check` pre-declares every signature)
    /// and a value is not, and nothing used to say so — a module with its constant
    /// table at the bottom failed at every function above it with a bare "not
    /// defined" at each use, one root cause wearing many faces.
    ///
    /// Granting `fn` bodies the same forward reference was tried and withdrawn. The
    /// tree-walker resolves a global at call time and would run it; the bytecode
    /// compiler binds global SLOTS at compile time and `LoadGlobal` has no
    /// initialised check, so the VM would read `Unit` where the walker raises. That
    /// is an engine divergence, and closing it means an uninitialised sentinel and a
    /// checked load on the interpreter's hottest opcode — its own change, with its
    /// own measurement, not a side effect of improving a message.
    deferred_globals: FxHashSet<String>,
    /// Every name declared by a top-level `fn` — the checker's twin of the compiler's
    /// `func_names` and the walker's `FuncVal::decl_name`, and the gate on typing a
    /// failed method call as a UFCS call.
    ///
    /// Deliberately the HOISTED set, not `fn_decls`: name resolution is file-scoped
    /// (ADR 0027), so `q.where(c)` above `fn where(q, c)` runs on both engines, and a
    /// checker that used the reached-so-far set would reject exactly that program.
    /// Shadowing is not read from here — `env` already answers it, because a later
    /// `where = 5` rebinds the name to a non-function type.
    fn_globals: FxHashSet<String>,
    /// Every checked `fn`'s parameters and body, for CALL-SITE SPECIALIZATION (field build,
    /// 1.44): a call whose arguments are informative re-types the body with them, so an
    /// argument-dependent shape — `fn mk(s) = {c: s.columns}` — reaches the call site. Kept
    /// behind an `Rc` so a specialization borrows nothing from the checker it mutates.
    fn_bodies: FxHashMap<String, std::rc::Rc<FnBody>>,
    /// Specializations already computed, by function and argument-type tuple: the precise
    /// return type, or `None` when the body did not type under those arguments (the
    /// definition's permissive answer stands — specialization adds precision, never a
    /// refusal).
    specialized: FxHashMap<String, Option<Type>>,
    /// The functions whose bodies are being specialized right now, by NAME: a call to one —
    /// under ANY argument types — answers the stored signature. By name and not by argument
    /// tuple because a structurally recursive function hands itself a strictly larger type
    /// each level (`_lassoc(ts, {node: {l: s.node, …}, …}, …)`, a precedence-climbing
    /// parser's left-association loop; field build, 1.48): a per-tuple guard never tripped,
    /// the nesting ran to the budget with linearly growing record types, and `helix check`
    /// went 18 ms → 3.7 s and 3.2 GB on a 617-line file. Monomorphic recursion is the
    /// classic rule (Hindley–Milner refuses polymorphic recursion without an annotation for
    /// the same reason); the answer at the cut is the permissive one, so nothing loses a
    /// program — only the precision a diverging recursion could never reach. The nesting is
    /// thereby bounded by the number of functions, and every type by the program's own
    /// structure. Pinned by `a_growing_recursion_is_specialized_once_per_chain`.
    specializing: FxHashSet<String>,
    /// How many specializations this check has run; past [`SPECIALIZATION_BUDGET`] the
    /// stored signature answers, so a pathological program cannot make `check` quadratic.
    specializations: usize,
    /// Whether receiver types are being recorded into [`Checker::types`]. Off during a
    /// specialization: the compiler routes each method call ONCE, by the definition's
    /// typing, and a call-site type must never overwrite that (the divergence the
    /// `mut_globals` note describes, arriving by another road).
    recording: bool,
    /// The destructure temporaries (`$rec<N>`) bound to a record LITERAL written right there.
    /// A destructure of such a temp is refused for a name the literal lacks (a typo, the case
    /// ADR 0046 wanted caught); a destructure of any other known shape answers `missing` for
    /// an absent field, as the form promises — with call-site specialization, a constructor's
    /// result is a known shape, and there absence is the normal case.
    literal_temps: FxHashSet<String>,
}

/// A checked function's parameters and body (see [`Checker::fn_bodies`]).
pub struct FnBody {
    params: Vec<(String, Option<TypeAnn>)>,
    body: Expr,
}

/// The most specializations one `check` runs. Each is one body typing; a real program uses
/// a handful, so the cap only ever trips on adversarial input.
const SPECIALIZATION_BUDGET: usize = 2_000;

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker {
    pub fn new() -> Self {
        let mut env = FxHashMap::default();
        for (name, _, _) in crate::interp::SEEDED_CONSTANTS {
            env.insert((*name).to_string(), Type::Float);
        }
        // The `python` interop entry point types as `Unknown`, so `python.import(...)`
        // and any method/attribute chain off a Python value never errors (Python
        // values ride the same permissive boundary as DataFrame columns).
        env.insert("python".to_string(), Type::Unknown);
        Checker {
            env,
            types: FxHashMap::default(),
            mut_globals: FxHashSet::default(),
            fn_decls: FxHashSet::default(),
            value_globals: crate::interp::seeded_names()
                .map(|s| s.to_string())
                .collect(),
            deferred_globals: FxHashSet::default(),
            fn_bodies: FxHashMap::default(),
            specialized: FxHashMap::default(),
            specializing: FxHashSet::default(),
            specializations: 0,
            recording: true,
            literal_temps: FxHashSet::default(),
            fn_globals: FxHashSet::default(),
        }
    }

    pub fn exec_stmt(&mut self, s: &Stmt) -> Result<(), HelixError> {
        match s {
            Stmt::Assign { name, mutable, value, line, col, .. } => {
                if name.starts_with("$rec") && matches!(value, Expr::Record(_)) {
                    self.literal_temps.insert(name.clone());
                }
                let t = self.synth(value)?;
                self.check_rebind(name, *mutable, *line, *col)?;
                if *mutable {
                    self.mut_globals.insert(name.clone());
                }
                self.value_globals.insert(name.clone());
                self.env.insert(name.clone(), t);
                Ok(())
            }
            Stmt::Destructure {
                names,
                mutable,
                value,
                line,
                col,
                ..
            } => {
                if *mutable {
                    for n in names {
                        self.mut_globals.insert(n.clone());
                    }
                }
                let t = self.synth(value)?;
                for n in names.iter() {
                    self.check_rebind(n, *mutable, *line, *col)?;
                    self.value_globals.insert(n.clone());
                }
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
                    Type::Unknown | Type::Missing | Type::Never => {
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
                defaults,
                ret,
                body,
                line,
                col,
                ..
            } => self.check_func(name, params, defaults, ret, body, *line, *col),
            Stmt::Expr(e) => {
                self.synth(e)?;
                Ok(())
            }
            // Stripped by the module loader before type-checking (see `Stmt::Import`).
            Stmt::Import { .. } => Ok(()),
        }
    }

    /// The static twin of the interpreter's `bind`: statements only occur
    /// straight-line at the top level, so a plain `name = ...` over an existing
    /// immutable VALUE binding — or `mut name = ...` over a reached `fn` —
    /// ALWAYS fails at run time. Say so at check time, with bind's exact
    /// wording. (`mut` over a value binding legally re-declares it as mutable,
    /// a duplicate `fn` is legal — first definition wins — and a plain assign
    /// over a `fn` legally shadows it, so none of those are flagged.)
    fn check_rebind(
        &self,
        name: &str,
        mutable: bool,
        line: usize,
        col: usize,
    ) -> Result<(), HelixError> {
        let clash = if mutable {
            self.fn_decls.contains(name)
        } else {
            self.value_globals.contains(name) && !self.mut_globals.contains(name)
        };
        if clash {
            let (msg, hint) = crate::error::immutable_reassign(name);
            return Err(HelixError::new(msg, line, col).hint(hint));
        }
        Ok(())
    }

    // As the comprehension compilers: a declaration's parts are all distinct, and bundling
    // them into a struct for the sake of a count would be shape for the count's sake.
    #[allow(clippy::too_many_arguments)]
    fn check_func(
        &mut self,
        name: &str,
        params: &[(String, Option<TypeAnn>)],
        defaults: &[Option<Expr>],
        ret: &Option<TypeAnn>,
        body: &Expr,
        line: usize,
        col: usize,
    ) -> Result<(), HelixError> {
        // A body that is EXACTLY a call to this function with its own parameters
        // unchanged recurses forever, whatever it is called with. The shape shows up
        // when a name shadows a builtin -- `fn relu(x) = relu(x)`, written to wrap the
        // builtin -- where the call resolves to the definition being written, not to
        // the builtin, and nothing said so until it hung at run time.
        if let Expr::Call { name: callee, args, line: cl, col: cc } = body
            && callee == name
            && args.len() == params.len()
            && args.iter().zip(params).all(|(a, (p, _))| {
                matches!(a, Expr::Ident { name: an, .. } if an == p)
            })
        {
            let err = HelixError::new(
                format!(
                    "`{name}` calls itself with the same arguments and nothing else, so it can never return"
                ),
                *cl,
                *cc,
            );
            return Err(if crate::registry::lookup(name).is_some() {
                err.hint(format!(
                    "`{name}` here is this definition, not the built-in of the same name \
                     — inside the body the name already refers to the function being \
                     defined. To wrap the builtin, give this one a different name."
                ))
            } else {
                err.hint(
                    "a recursive function needs a base case, and needs to make progress \
                     towards it — recurse on a smaller argument.",
                )
            });
        }

        let param_types: Vec<Type> = params
            .iter()
            .map(|(_, ann)| ann.as_ref().map(ann_to_type).unwrap_or(Type::Unknown))
            .collect();
        let ret_ann = ret.as_ref().map(ann_to_type);

        // A `fn` over an existing immutable VALUE binding is bind's
        // immutable-reassign error (over a `mut` binding it legally rebinds).
        self.check_rebind(name, false, line, col)?;
        // Mirror the runtime's `fn_decls`: from here on, `mut name = ...` is
        // the immutable-reassign error (`bind` refuses `mut` over a reached fn).
        self.fn_decls.insert(name.to_string());
        // Insert a provisional signature BEFORE checking the body so recursive
        // self-calls type (as Unknown return) instead of "not defined".
        self.env.insert(
            name.to_string(),
            Type::Function {
                params: param_types.clone(),
                ret: Box::new(ret_ann.clone().unwrap_or(Type::Unknown)),
                required: params.len() - defaults.iter().flatten().count(),
                origin: Some(std::rc::Rc::from(name)),
            },
        );

        let body_t = self.type_body(name, params, &param_types, body)?;

        if let Some(rt) = &ret_ann
            && !annotation_admits(rt, &body_t) {
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

        // Keep the body for call-site specialization (field build, 1.44).
        self.fn_bodies.insert(
            name.to_string(),
            std::rc::Rc::new(FnBody { params: params.to_vec(), body: body.clone() }),
        );
        // Store the final signature (inferred return if not annotated).
        let final_ret = ret_ann.unwrap_or(body_t);
        self.env.insert(
            name.to_string(),
            Type::Function {
                params: param_types,
                ret: Box::new(final_ret),
                required: params.len() - defaults.iter().flatten().count(),
                origin: Some(std::rc::Rc::from(name)),
            },
        );
        Ok(())
    }

    /// Type a function body with its parameters bound to `param_types` — the definition's
    /// (annotations, else `Unknown`) from `check_func`, or a call site's from `specialize`.
    /// A `mut` global types as Unknown inside the deferred body (see `mut_globals`): the
    /// body runs at call time, by which the global may hold a different type. The fn's own
    /// name is exempt — the definition rebinds it, and self-calls should see the provisional
    /// signature. Snapshot BEFORE the param save so a same-named param restores the Unknown
    /// set here, and the restore below puts the real type back.
    fn type_body(
        &mut self,
        name: &str,
        params: &[(String, Option<TypeAnn>)],
        param_types: &[Type],
        body: &Expr,
    ) -> Result<Type, HelixError> {
        let saved_muts: Vec<(String, Type)> = self
            .mut_globals
            .iter()
            .filter(|n| n.as_str() != name)
            .filter_map(|n| self.env.get(n).map(|t| (n.clone(), t.clone())))
            .collect();
        for (n, _) in &saved_muts {
            self.env.insert(n.clone(), Type::Unknown);
        }
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
        for (n, t) in saved_muts {
            self.env.insert(n, t);
        }
        body_result
    }

    /// CALL-SITE SPECIALIZATION (field build, 1.44). A call to a checked `fn` whose
    /// unannotated parameters receive informative argument types re-types the body with
    /// those types bound (annotated parameters keep their annotation), and answers the
    /// result when it is more precise than the stored return type. Memoized per function and
    /// argument-type tuple; a call to a function whose body is being specialized — under any
    /// arguments — answers the stored signature (the in-progress guard, by name: see
    /// `specializing`); the receiver side-table is not written
    /// (`recording`), so the compiler's routing stays the definition's. A body that does not
    /// type under the call's arguments — a branch the arguments would never take, typed all
    /// the same — answers `None`, and the caller keeps the definition's permissive return:
    /// this adds precision, and never rejects a program that ran.
    pub(super) fn specialize(
        &mut self,
        name: &str,
        params: &[Type],
        args: &[Type],
        origin: &Option<std::rc::Rc<str>>,
    ) -> Option<Type> {
        // Only the function that OWNS the body: a local of the same name is not it.
        if origin.as_deref() != Some(name) {
            return None;
        }
        let fb = self.fn_bodies.get(name)?.clone();
        let mut bind: Vec<Type> = Vec::with_capacity(params.len());
        let mut informative = false;
        for (i, p) in params.iter().enumerate() {
            // An UNANNOTATED parameter takes the argument's type; an annotated one keeps its
            // annotation — and `x: Any` is therefore the way to keep a function opaque on
            // purpose (a laundering `fn launder(x: Any) = x` stays a laundering).
            let unannotated = fb.params.get(i).is_some_and(|(_, ann)| ann.is_none());
            bind.push(match (p, args.get(i)) {
                (Type::Unknown, Some(a)) if unannotated && !matches!(a, Type::Unknown | Type::Missing | Type::Never) => {
                    informative = true;
                    a.clone()
                }
                _ => p.clone(),
            });
        }
        if !informative {
            return None;
        }
        let key = format!("{name}|{bind:?}");
        if let Some(memo) = self.specialized.get(&key) {
            return memo.clone();
        }
        if self.specializing.contains(name) || self.specializations >= SPECIALIZATION_BUDGET {
            return None;
        }
        self.specializations += 1;
        self.specializing.insert(name.to_string());
        let was_recording = self.recording;
        self.recording = false;
        let result = self.type_body(name, &fb.params, &bind, &fb.body);
        self.recording = was_recording;
        self.specializing.remove(name);
        let out = match result {
            Ok(t) if !matches!(t, Type::Unknown) => Some(t),
            _ => None,
        };
        self.specialized.insert(key, out.clone());
        out
    }

    /// Whether `e` reads a field a known record shape PROVABLY holds a value for — `r.k`, or
    /// `r.get("k")`, with `r` a name bound to a Record whose `k` is a value type (not
    /// Missing, Unknown or Never). Such a read never answers `missing`, so a `?? d` after it
    /// never applies and the whole has the field's type: `spec.columns ?? []` keeps the
    /// shape `spec.columns` has (field build, 1.44a). Anything else joins with the default,
    /// as before — the checker does not track which OTHER reads may answer `missing`.
    fn reads_present_field(&self, e: &Expr) -> bool {
        let (recv, key): (&Expr, &str) = match e {
            Expr::Field { recv, name, .. } => (recv, name.as_str()),
            Expr::Method { recv, name, args, .. } if name == "get" && args.len() <= 2 => {
                match args.first() {
                    Some(Expr::Str(k)) => (recv, k.as_str()),
                    _ => return false,
                }
            }
            _ => return false,
        };
        let Expr::Ident { name: r, .. } = recv else {
            return false;
        };
        match self.env.get(r) {
            Some(Type::Record(fields)) => fields
                .iter()
                .any(|(f, t)| f == key && !matches!(t, Type::Missing | Type::Unknown | Type::Never)),
            _ => false,
        }
    }

    fn synth(&mut self, e: &Expr) -> Result<Type, HelixError> {
        match e {
            Expr::Int(_) => Ok(Type::Int),
            Expr::Float(_) => Ok(Type::Float),
            Expr::Str(_) => Ok(Type::String),
            Expr::Bool(_) => Ok(Type::Bool),
            Expr::Missing => Ok(Type::Missing),
            Expr::Column { name, line, col } => Err(HelixError::new(
                format!("`@{name}` is a column reference, only valid inside a DataFrame operation"),
                *line,
                *col,
            )
            .hint("use `@column` inside a verb like `df.where(...)`, `df.select(...)`, or `df.group(...)`.")),
            Expr::Interp(parts) => {
                // Type-check every embedded expression (so `"{undefined}"` errors),
                // then the whole thing is a String. An undefined BARE NAME in a hole
                // gets the extra half of the story: the braces themselves may be the
                // surprise (the author may have wanted literal text), so the hint
                // teaches both the rule and the `{{ }}` escape — same house pattern as
                // `try_binds_tighter_hint`, and like it, it never displaces a more
                // specific hint and keys on the AST node, not the message text.
                for part in parts {
                    if let crate::ast::InterpPart::Expr(e, _) = part {
                        self.synth(e).map_err(|err| interp_hole_hint(err, e))?;
                    }
                }
                Ok(Type::String)
            }
            Expr::Ident { name, line, col } => match self.env.get(name) {
                Some(t) => Ok(t.clone()),
                // A name that collides with a removed namespace (`stats`, `io`, `bio`,
                // …) used as a bare value. It's simply undefined — but because the name
                // used to be a built-in prefix, say so and point at both escape hatches:
                // the old members are functions/methods now, and a same-named *module*
                // just needs importing. (A successful `import bio` is rewritten by the
                // module loader before it reaches here, so this only fires when unbound.)
                None if crate::namespace::is_namespace(name) => Err(HelixError::new(
                    format!("`{name}` is not defined"),
                    *line,
                    *col,
                )
                .hint(format!(
                    "`{name}` is no longer a built-in namespace (ADR 0017) — its old members \
                     are now free functions (e.g. `read_csv`) or methods (e.g. `value.to_json()`). \
                     If you meant a module named `{name}`, import it first: `import {name}`."
                ))),
                // A registry builtin used as a bare VALUE (`f = print`): the old
                // message said "not defined … assign it first, e.g. `print = ...`"
                // — circular advice for a name that IS defined as a callable.
                None if crate::registry::lookup(name).is_some() => Err(HelixError::new(
                    format!("built-in `{name}` cannot be used as a value"),
                    *line,
                    *col,
                )
                .hint(format!("wrap it in a function instead: `f = (x => {name}(x))`."))),
                // `it` outside a comprehension body (e.g. inside a nested `=>`
                // lambda, which binds the element to its own parameter): the
                // Levenshtein pass used to suggest the math constant `e`.
                None if name == "it" => Err(HelixError::new(
                    "`it` is not defined here",
                    *line,
                    *col,
                )
                .hint("`it` is the implicit element inside a comprehension body; a `=>` function receives the element as its own parameter — write `.map(x => ...)`.")),
                // Bound at the top level, but BELOW this use. A `fn` may be used
                // above its definition and a value may not, and nothing said so --
                // the message named the use and left the reader to find the cause.
                None if self.deferred_globals.contains(name) => Err(HelixError::new(
                    format!("`{name}` is not defined yet"),
                    *line,
                    *col,
                )
                .hint(format!(
                    "`{name}` is bound further down this file. Unlike a `fn`, which may \
                     be called above its definition, a top-level value binding has to \
                     appear before the code that uses it — move the binding up."
                ))),
                None => {
                    let names: Vec<&str> = self.env.keys().map(|s| s.as_str()).collect();
                    let err = HelixError::new(format!("`{}` is not defined", name), *line, *col);
                    // No fallback: the old one told people to `assign it first, e.g.
                    // `None = ...``, which is advice to define a variable named `None`
                    // rather than to write `missing`. Silence beats that.
                    Err(match crate::suggest::hint(name, crate::suggest::Site::Value, &names) {
                        Some(h) => err.hint(h),
                        None => err,
                    })
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
            Expr::RecordUpdate { base, fields, line, col } => {
                let base_t = self.synth(base)?;
                let mut updates = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    updates.push((k.clone(), self.synth(v)?));
                }
                match base_t {
                    // A statically-known record: merge the update fields (override or extend)
                    // so field access on the result is still precisely checked.
                    Type::Record(mut tys) => {
                        for (name, ty) in updates {
                            match tys.iter_mut().find(|(k, _)| *k == name) {
                                Some(slot) => slot.1 = ty,
                                None => tys.push((name, ty)),
                            }
                        }
                        Ok(Type::Record(tys))
                    }
                    // The base's shape isn't known (a `parse_json` result, a parameter, …).
                    // The result is a record, but its full field set can't be proven — stay
                    // permissive, exactly as for dynamic field access.
                    // A DICT lands here too: it is `Unknown` to the checker (the
                    // opaque-type pattern), and its keys are not known statically, so a
                    // spread of one is a record whose field set cannot be proven —
                    // which is exactly what this arm already answers.
                    Type::Unknown | Type::Missing | Type::AnyRecord | Type::Never => Ok(Type::Unknown),
                    other => Err(HelixError::new(
                        format!("`...` record update needs a record, got {other}"),
                        *line,
                        *col,
                    )
                    .hint("the spread base must be a record or a dict, e.g. `{ ...resp, status: 500 }`.")),
                }
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
                        .ok_or_else(|| record_has_no_field(fields, name, *line, *col)),
                    // An annotated `Record` (or `Any`) is open: the field's type is not known.
                    Type::Unknown | Type::Missing | Type::AnyRecord | Type::Never => Ok(Type::Unknown),
                    other => Err(field_on_non_record(other, name, *line, *col)),
                }
            }
            // A destructured field (`let {a} = e in …`; see the parser's `destructure_record`).
            // Where the record is a LITERAL written right there, a name it lacks is a typo and
            // is refused in the words `.a` uses; where the shape is known some other way — a
            // constructor's result, through call-site specialization — an absent field is
            // `missing`, as the form promises (absence is a spec record's normal case, ADR
            // 0046's own words); where the shape is not known, the read is `Unknown`. A
            // receiver the checker can prove has no fields at all is refused here rather than
            // at run time.
            Expr::FieldOrMissing { recv, name, line, col } => {
                let rt = self.synth(recv)?;
                let from_literal =
                    matches!(&**recv, Expr::Ident { name: t, .. } if self.literal_temps.contains(t));
                match &rt {
                    Type::Record(fields) => match fields.iter().find(|(k, _)| k == name) {
                        Some((_, t)) => Ok(t.clone()),
                        None if from_literal => Err(record_has_no_field(fields, name, *line, *col)),
                        None => Ok(Type::Missing),
                    },
                    Type::Unknown | Type::Missing | Type::AnyRecord | Type::Never => Ok(Type::Unknown),
                    other => Err(HelixError::new(
                        format!(
                            "cannot destructure {}: it has no fields",
                            crate::value::with_article(&other.to_string())
                        ),
                        *line,
                        *col,
                    )
                    .hint("destructuring reads the fields of a record, or the keys of a dict.")),
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
                // `x ?? d` where `x` reads a field the shape provably holds a value for: the
                // default never applies, so the whole has the field's type (field build,
                // 1.44a — `spec.columns ?? []` keeps the shape `spec.columns` has).
                if matches!(op, BinOp::Coalesce) && self.reads_present_field(left) {
                    return Ok(lt);
                }
                self.synth_binary(op, &lt, &rt, *line, *col)
                    .map_err(|e| try_binds_tighter_hint(e, op, left, right))
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
                named,
                ufcs,
                line,
                col,
            } => {
                // A `Method` still carrying named arguments here is a genuine method call on a
                // value (a qualified module call was rewritten to a resolved `Call` by the
                // loader before type-checking). Named arguments bind to a declared function's
                // parameter names, which a value's method doesn't expose — so reject them.
                if !named.is_empty() {
                    return Err(HelixError::new(
                        "named arguments are not supported on method calls",
                        *line,
                        *col,
                    )
                    .hint("only functions take named arguments; pass method arguments positionally."));
                }
                self.synth_method(recv, name, ufcs.as_deref(), args, *line, *col)
            }
            Expr::CallValue { callee, args, .. } => {
                // Calling a first-class function *value* — its parameter/return types
                // aren't tracked statically (functions live in records/arrays as opaque
                // values). Check the callee and args for their own errors, then yield
                // Unknown, matching the permissive treatment of dynamic access.
                self.synth(callee)?;
                for a in args {
                    self.synth(a)?;
                }
                Ok(Type::Unknown)
            }
            Expr::Index {
                recv,
                index,
                line,
                col,
            } => {
                let rt = self.synth(recv)?;
                let it = self.synth(index)?;
                // `r["key"]` — a string index is dynamic record-field access, allowed
                // on records and Unknown (e.g. a `parse_json` result). The key is
                // dynamic, so the result is Unknown; an absent key is `missing` at
                // runtime.
                if matches!(it, Type::String) {
                    return match rt {
                        Type::Record(_) | Type::AnyRecord | Type::Dict | Type::Unknown | Type::Missing | Type::Never => Ok(Type::Unknown),
                        other => Err(HelixError::new(
                            format!("a value of type {} cannot be indexed by a string", other),
                            *line,
                            *col,
                        )
                        .hint("string indexing `r[\"key\"]` works on records.")),
                    };
                }
                // otherwise the index must be an integer (Unknown/Missing pass)
                if !compatible(&it, &Type::Int) {
                    return Err(type_err("index", "an integer", &it, *line, *col));
                }
                Ok(match rt {
                    Type::Array(el) => *el,
                    // index is dynamic, so a tuple element is the join of all
                    // element types (precise when homogeneous, e.g. `(Int, Int)`).
                    Type::Tuple(els) => els.iter().fold(Type::Missing, |a, t| join(&a, t)),
                    Type::String | Type::Dna => Type::String,
                    // A record indexed by a *dynamic* key (`CODE[codon]`, key type not
                    // statically `String`) is runtime field access — the field value, or
                    // `missing` if absent. The result type isn't known, so `Unknown`. (The
                    // static-string-key case is handled above.) The checker must not reject
                    // it: the identical access runs fine, per the never-reject-runnable rule.
                    Type::Record(_) | Type::AnyRecord | Type::Dict | Type::AnyTuple => Type::Unknown,
                    Type::Unknown | Type::Missing | Type::Tensor | Type::Never => Type::Unknown,
                    other => {
                        let err = HelixError::new(
                            format!("a value of type {} cannot be indexed", other),
                            *line,
                            *col,
                        );
                        // `df[0]` is the first thing a pandas user types, and it was the
                        // largest single no-help shape in the adversarial sweep. A frame is
                        // columnar and lazy — there is no row sitting there to hand back —
                        // so name the two verbs that do what was meant.
                        return Err(match other {
                            Type::DataFrame => err.hint(
                                "a DataFrame is columnar and lazy — take rows with `df.head(n)` \
                                 and a column with `df.column(\"name\")`.",
                            ),
                            _ => err,
                        });
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
                    Type::Unknown | Type::Missing | Type::Tensor | Type::Never => Type::Unknown,
                    other => {
                        return Err(HelixError::new(
                            format!("a value of type {} cannot be sliced", other),
                            *line,
                            *col,
                        )
                        .hint("slicing works on arrays, strings, DNA, and tensors (first axis)."))
                    }
                })
            }
            Expr::Lambda { params, anns, defaults, body, .. } => {
                // Standalone lambda: an annotated parameter takes its annotation (`(x: Int)
                // => x`, field build 1.45b); the rest default to Unknown. Like a `fn`
                // body (see `check_func`), the lambda body is deferred — a
                // `mut` global read inside it types as Unknown, since the
                // global may be rebound before the lambda is called.
                let saved_muts: Vec<(String, Type)> = self
                    .mut_globals
                    .iter()
                    .filter_map(|n| self.env.get(n).map(|t| (n.clone(), t.clone())))
                    .collect();
                for (n, _) in &saved_muts {
                    self.env.insert(n.clone(), Type::Unknown);
                }
                let saved: Vec<(String, Option<Type>)> = params
                    .iter()
                    .map(|n| (n.clone(), self.env.get(n).cloned()))
                    .collect();
                let param_types: Vec<Type> = params
                    .iter()
                    .enumerate()
                    .map(|(i, _)| anns.get(i).and_then(|a| a.as_ref()).map(ann_to_type).unwrap_or(Type::Unknown))
                    .collect();
                for (n, t) in params.iter().zip(&param_types) {
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
                for (n, t) in saved_muts {
                    self.env.insert(n, t);
                }
                let body_t = body_result?;
                Ok(Type::Function {
                    params: param_types,
                    ret: Box::new(body_t),
                    required: params.len() - defaults.len(),
                    origin: None,
                })
            }
            Expr::Let { bindings, body, .. } => {
                let mut saved: Vec<(String, Option<Type>)> = Vec::with_capacity(bindings.len());
                for (name, expr) in bindings {
                    if name.starts_with("$rec") && matches!(expr, Expr::Record(_)) {
                        self.literal_temps.insert(name.clone());
                    }
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
                if !matches!(ct, Type::Bool | Type::Missing | Type::Unknown | Type::Never) {
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
            // `try EXPR` yields `{ok: Bool, value: <EXPR's type>, error: String}`. A
            // type error inside EXPR is still reported (try catches runtime errors,
            // not compile-time ones). On the error path `value` is `missing`, which is
            // compatible with the success type, so field access stays sound.
            Expr::Try { expr, line, col } => {
                // `try` takes an EXPRESSION and evaluates it; every other language's
                // equivalent takes a callback, so `try(() => f())` is what a newcomer
                // writes -- and it quietly succeeded, because building a closure cannot
                // fail. The record came back `{ok: true, value: <function/0>}` and the
                // error handling never fired. There is no reading under which wrapping
                // a function literal in `try` is useful, so refuse it and say what to
                // write instead.
                if matches!(&**expr, Expr::Lambda { .. }) {
                    return Err(HelixError::new(
                        "`try` takes an expression to evaluate, not a function",
                        *line,
                        *col,
                    )
                    .hint(
                        "building a function never fails, so this would always report \
                         success. Call it inside the `try` instead: `try f()`, or \
                         `try (f(x))`.",
                    ));
                }
                let vt = match self.synth(expr) {
                    Ok(t) => t,
                    Err(e) => {
                        // `try(f()).ok` parses as `try` OF `f().ok` — the postfix
                        // chain binds tighter than `try`. When the inner failure is
                        // exactly a missing `ok`/`value`/`error` field, the generic
                        // "field access works on records" hint is the WRONG lesson —
                        // replace it with the one naming the real rule.
                        if let Expr::Field { name, .. } = &**expr
                            && matches!(name.as_str(), "ok" | "value" | "error")
                            && e.message.contains(&format!("has no field `{name}`"))
                        {
                            let mut e = e;
                            e.hint = Some(format!(
                                "`try` binds tighter than `.{name}`, so this reads \
                                 `.{name}` on the inner value. Bind the result first: \
                                 `let r = try(...) in r.{name}`."
                            ));
                            return Err(e);
                        }
                        return Err(e);
                    }
                };
                Ok(Type::Record(vec![
                    ("ok".to_string(), Type::Bool),
                    ("value".to_string(), vt),
                    ("error".to_string(), Type::String),
                    ("help".to_string(), Type::String),
                ]))
            }
            Expr::Match { scrutinee, arms, .. } => {
                let _ = self.synth(scrutinee)?;
                let mut result: Option<Type> = None;
                for arm in arms {
                    // A pattern's bound names are in scope for the guard and the body.
                    // Their precise types depend on the (possibly nested) pattern
                    // position, so the permissive checker gives each `Unknown`.
                    let names = crate::interp::pattern_binding_names(&arm.pattern);
                    let saved: Vec<(String, Option<Type>)> = names
                        .iter()
                        .map(|n| (n.clone(), self.env.insert(n.clone(), Type::Unknown)))
                        .collect();
                    if let Some(g) = &arm.guard {
                        let _ = self.synth(g)?; // surfaces type errors inside the guard
                    }
                    let bt = self.synth(&arm.body)?;
                    for (n, prev) in saved.into_iter().rev() {
                        match prev {
                            Some(t) => {
                                self.env.insert(n, t);
                            }
                            None => {
                                self.env.remove(&n);
                            }
                        }
                    }
                    result = Some(match result {
                        Some(r) => join(&r, &bt),
                        None => bt,
                    });
                }
                Ok(result.unwrap_or(Type::Unknown))
            }
        }
    }

}


mod signatures;
use signatures::*;

/// Type-check a whole program. Runs after parse, before interpretation.
///
/// TWO PASSES, and the first one is why mutual recursion works. `check_func` already inserts
/// a provisional signature before checking its own body, so a function can call ITSELF; doing
/// the same for every top-level function before checking ANY body is the whole generalization,
/// and it is what turns
///
///     fn even(n) = if n == 0 then true else odd(n - 1)
///     fn odd(n)  = if n == 0 then false else even(n - 1)
///
/// from `error: \`odd\` is not a known function` into a program. Before this, the checker
/// walked statements in order and a name entered `env` only when its own statement was
/// reached — so a body could never mention a peer defined below it, which rules out
/// recursive-descent parsers, mutually recursive tree walkers, and state machines whose
/// states reference each other. That is most of "a library that is not numerics".
///
/// The signature registered here is exactly the one `check_func` would register: annotations
/// where written, `Unknown` where not. `check_func` then overwrites it with the same value
/// when it reaches the definition, so nothing downstream can tell the difference — this pass
/// only makes the name VISIBLE earlier.
///
/// A later `fn` of the same name still wins, as it did before: the second registration
/// overwrites the first, in source order, in both passes.
pub fn check(program: &[Stmt]) -> Result<TypeMap, HelixError> {
    check_program(program).map(|c| c.types)
}

/// [`check`]'s driver, answering the checker itself: its `specializations` counter is what
/// the test of the specialization's bound reads.
fn check_program(program: &[Stmt]) -> Result<Checker, HelixError> {
    let mut checker = Checker::new();
    // Declare every top-level `fn` before checking any body, so a body may reference a peer
    // defined below it (`fn even` calling `fn odd`) without the checker rejecting the file
    // ahead of the engines. Only the SIGNATURE is declared here; bodies are checked in order
    // below, so nothing is inferred from a definition that has not been read yet.
    //
    // A name that shadows a builtin is skipped, for the same reason the bytecode pre-pass
    // skips it: the shadow is not retroactive, and declaring it here would type calls ABOVE
    // the definition against the user's function — a wrong type handed to the JIT, not just a
    // permissive check.
    for s in program {
        if let Stmt::Func { name, params, defaults, ret, .. } = s
            && crate::registry::lookup(name).is_none()
        {
            let param_types: Vec<Type> = params
                .iter()
                .map(|(_, ann)| ann.as_ref().map(ann_to_type).unwrap_or(Type::Unknown))
                .collect();
            let ret_ty = ret.as_ref().map(ann_to_type).unwrap_or(Type::Unknown);
            checker.env.insert(
                name.clone(),
                Type::Function {
                    params: param_types,
                    ret: Box::new(ret_ty),
                    required: params.len() - defaults.iter().flatten().count(),
                    origin: Some(std::rc::Rc::from(name.as_str())),
                },
            );
            checker.fn_globals.insert(name.clone());
        }
    }
    // The same courtesy for top-level VALUE bindings, and for the same reason: a
    // deferred body runs after the whole file has executed, so a constant table at
    // the bottom is in scope for every function above it. Recorded as names only --
    // `check_func` hoists them into the body's environment as `Unknown` and takes
    // them out again, so top-level flow keeps rejecting a genuine use-before-binding.
    for s in program {
        if let Stmt::Assign { name, .. } = s {
            checker.deferred_globals.insert(name.clone());
        }
    }
    for s in program {
        checker.exec_stmt(s)?;
    }
    Ok(checker)
}


#[cfg(test)]
mod tests;

mod synth;
