//! A column expression is a value (ADR 0052).
//!
//! `@age > 30` inside a frame verb — `df.where(@age > 30)` — is how Helix spells a
//! condition, and the frame reads it as an expression over its columns. Outside a frame verb
//! it was a check-time error. It is a VALUE now: the record that describes it,
//!
//! ```text
//! {kind: "bin", op: ">", left: {kind: "col", name: "age"}, right: {kind: "lit", value: 30}}
//! ```
//!
//! which a library reads like any record — an ORM renders `age > $1` from it and binds `30`
//! — and which a frame verb accepts back through a name: `p = @age > 30; df.where(p)`. One
//! spelling for a condition wherever it goes, and one the load-time specializer (ADR 0051)
//! sees through: the column, the operator and the shape are literals in the record, only
//! the value is the request's, so a library's rendering of it is a load-time constant.
//!
//! The encoding mirrors the frame engine's own `ColExpr`, node for node:
//!
//! | expression                       | record                                                        |
//! |----------------------------------|---------------------------------------------------------------|
//! | `@name`                          | `{kind: "col", name: "name"}`                                 |
//! | a literal, a name, a call        | `{kind: "lit", value: v}`                                     |
//! | `a > b`, `a + b`, `a and b`, …   | `{kind: "bin", op: ">", left: a, right: b}` (the operator's spelling) |
//! | `not a`, `-a`                    | `{kind: "not", expr: a}`, `{kind: "neg", expr: a}`            |
//! | `a.is_missing()`                 | `{kind: "is_missing", expr: a}`                               |
//! | `a.is_nan()`, `is_finite(a)`     | `{kind: "is_nan", expr: a}`, `{kind: "is_finite", expr: a}`   |
//! | `a.starts_with("x")` and the other String tests | `{kind: "str", name: "starts_with", expr: a, args: ["x"]}` |
//!
//! The rewrite is the parser's, on the finished tree: a column expression anywhere but in
//! the argument list of a method named like a frame verb (`where`, `filter`, `select`,
//! `sort`, `group`, `with`, `join`, `count`, …) becomes its record; a frame verb's arguments
//! stay as written, for the frame reading. A record's method of such a name receives a
//! predicate through a binding — the argument position is the frame's.

use std::rc::Rc;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, UnOp};
use crate::backend::{float_pred_kind, str_fn, validate_scalar, ColExpr};
use crate::error::HelixError;
use crate::symbol::Symbol;
use crate::value::Value;

/// The kinds a predicate record's `kind` field names.
pub const KINDS: &[&str] = &["col", "lit", "bin", "not", "neg", "is_missing", "is_nan", "is_finite", "str"];

/// Whether `e` is a column expression: a `@name`, or one reached through what a frame
/// reads — comparison, arithmetic, `and`/`or`/`not`, `is_missing()`, `is_nan()`,
/// `is_finite()` and the String tests. `df.where(@a > 1).count() > 3` is not one: the
/// column sits under a verb, and `count()` is a value.
pub fn is_column_expr(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } => true,
        Expr::Binary { left, right, .. } => is_column_expr(left) || is_column_expr(right),
        Expr::Unary { expr, .. } => is_column_expr(expr),
        Expr::Method { recv, name, args, .. } => {
            let reads = (args.is_empty() && (name == "is_missing" || float_pred_kind(name).is_some())) || str_fn(name).is_some();
            reads && is_column_expr(recv)
        }
        Expr::Call { name, args, .. } => float_pred_kind(name).is_some() && args.len() == 1 && is_column_expr(&args[0]),
        _ => false,
    }
}

fn field(k: &str, v: Expr) -> (String, Expr) {
    (k.to_string(), v)
}

fn text(t: &str) -> Expr {
    Expr::Str(t.to_string())
}

/// The record a column expression is as a value. A leaf that is not a column expression —
/// a literal, a name, a call — is `{kind: "lit", value: …}`, evaluated where it stands, with
/// any column expression inside it made a value in turn.
pub fn to_record(e: &Expr) -> Result<Expr, HelixError> {
    let fields = match e {
        Expr::Column { name, .. } => vec![field("kind", text("col")), field("name", text(name))],
        Expr::Binary { op, left, right, .. } if is_column_expr(e) => vec![
            field("kind", text("bin")),
            field("op", text(op.symbol())),
            field("left", to_record(left)?),
            field("right", to_record(right)?),
        ],
        Expr::Unary { op, expr, .. } if is_column_expr(e) => vec![
            field("kind", text(match op {
                UnOp::Not => "not",
                UnOp::Neg => "neg",
            })),
            field("expr", to_record(expr)?),
        ],
        Expr::Method { recv, name, .. } if is_column_expr(e) && name == "is_missing" => {
            vec![field("kind", text("is_missing")), field("expr", to_record(recv)?)]
        }
        Expr::Method { recv, name, args, .. } if is_column_expr(e) && args.is_empty() && float_pred_kind(name).is_some() => {
            vec![field("kind", text(name)), field("expr", to_record(recv)?)]
        }
        Expr::Call { name, args, .. } if is_column_expr(e) => vec![field("kind", text(name)), field("expr", to_record(&args[0])?)],
        Expr::Method { recv, name, args, .. } if is_column_expr(e) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                let mut v = a.clone();
                rewrite(&mut v)?;
                vals.push(v);
            }
            vec![
                field("kind", text("str")),
                field("name", text(name)),
                field("expr", to_record(recv)?),
                field("args", Expr::Array(vals)),
            ]
        }
        other => {
            let mut v = other.clone();
            rewrite(&mut v)?;
            vec![field("kind", text("lit")), field("value", v)]
        }
    };
    Ok(Expr::Record(fields))
}

/// Make every column expression of the program a value, except in the argument lists of
/// frame verbs — the parser's last pass over the finished tree.
pub fn desugar_program(stmts: &mut [Stmt]) -> Result<(), HelixError> {
    for s in stmts.iter_mut() {
        match s {
            Stmt::Assign { value, .. } | Stmt::Destructure { value, .. } | Stmt::Expr(value) => rewrite(value)?,
            Stmt::Func { defaults, body, .. } => {
                for d in defaults.iter_mut().flatten() {
                    rewrite(d)?;
                }
                rewrite(body)?;
            }
            Stmt::Import { .. } => {}
        }
    }
    Ok(())
}

fn rewrite(e: &mut Expr) -> Result<(), HelixError> {
    if is_column_expr(e) {
        let r = to_record(e)?;
        *e = r;
        return Ok(());
    }
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing | Expr::Ident { .. } | Expr::Column { .. } => {}
        Expr::Interp(parts) => {
            for p in parts {
                if let InterpPart::Expr(x, _) = p {
                    rewrite(x)?;
                }
            }
        }
        Expr::Array(xs) | Expr::Tuple(xs) => {
            for x in xs {
                rewrite(x)?;
            }
        }
        Expr::Record(fields) => {
            for (_, v) in fields {
                rewrite(v)?;
            }
        }
        Expr::RecordUpdate { parts, .. } => {
            for p in parts {
                rewrite(p.expr_mut())?;
            }
        }
        Expr::Field { recv, .. } | Expr::FieldOrMissing { recv, .. } | Expr::Unary { expr: recv, .. } | Expr::Try { expr: recv, .. } => {
            rewrite(recv)?
        }
        Expr::Binary { left, right, .. } => {
            rewrite(left)?;
            rewrite(right)?;
        }
        Expr::Call { args, .. } => {
            for a in args {
                rewrite(a)?;
            }
        }
        Expr::Method { recv, name, args, named, .. } => {
            rewrite(recv)?;
            // A frame verb reads its arguments as written: a column expression there is
            // the frame's, not a value.
            if !crate::interp::takes_unevaluated_args(name) {
                for a in args {
                    rewrite(a)?;
                }
                for (_, v) in named {
                    rewrite(v)?;
                }
            }
        }
        Expr::CallValue { callee, args, .. } => {
            rewrite(callee)?;
            for a in args {
                rewrite(a)?;
            }
        }
        Expr::Index { recv, index, .. } => {
            rewrite(recv)?;
            rewrite(index)?;
        }
        Expr::Slice { recv, start, stop, step, .. } => {
            rewrite(recv)?;
            for p in [start, stop, step].into_iter().flatten() {
                rewrite(p)?;
            }
        }
        Expr::Lambda { defaults, bound, body, .. } => {
            for d in defaults {
                rewrite(d)?;
            }
            if let Some(o) = bound {
                rewrite(o)?;
            }
            if let Some(b) = Rc::get_mut(body) {
                rewrite(b)?;
            }
        }
        Expr::Let { bindings, body, .. } => {
            for (_, v) in bindings {
                rewrite(v)?;
            }
            rewrite(body)?;
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            rewrite(cond)?;
            rewrite(then_branch)?;
            rewrite(else_branch)?;
        }
        Expr::Match { scrutinee, arms, .. } => {
            rewrite(scrutinee)?;
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    rewrite(g)?;
                }
                rewrite(&mut arm.body)?;
            }
        }
    }
    Ok(())
}

fn get<'a>(fields: &'a [(Symbol, Value)], k: &str) -> Option<&'a Value> {
    fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v)
}

/// The `kind` of `v` when it is a predicate record — a record whose `kind` names one of
/// the kinds above.
fn kind_of(v: &Value) -> Option<&str> {
    let Value::Record(fields) = v else { return None };
    match get(fields, "kind") {
        Some(Value::Str(k)) if KINDS.contains(&k.as_str()) => Some(k.as_str()),
        _ => None,
    }
}

/// Whether `v` is a predicate record.
pub fn is_predicate(v: &Value) -> bool {
    kind_of(v).is_some()
}

fn binop_from_symbol(sym: &str) -> Option<BinOp> {
    const ALL: [BinOp; 21] = [
        BinOp::Add,
        BinOp::Sub,
        BinOp::Mul,
        BinOp::Div,
        BinOp::FloorDiv,
        BinOp::Mod,
        BinOp::Pow,
        BinOp::Eq,
        BinOp::Ne,
        BinOp::Lt,
        BinOp::Gt,
        BinOp::Le,
        BinOp::Ge,
        BinOp::And,
        BinOp::Or,
        BinOp::Coalesce,
        BinOp::BitAnd,
        BinOp::BitOr,
        BinOp::BitXor,
        BinOp::Shl,
        BinOp::Shr,
    ];
    ALL.iter().find(|op| op.symbol() == sym).cloned()
}

/// The expression a predicate record describes, for a frame verb handed the value through
/// a name — `df.where(p)`. The inverse of `to_record`, with the frame's own checks: a
/// column must exist, a literal must be a scalar.
pub fn from_value(v: &Value, columns: &[String], line: usize, col: usize) -> Result<ColExpr, HelixError> {
    let Value::Record(fields) = v else { return Err(not_a_predicate(line, col)) };
    let kind = kind_of(v).ok_or_else(|| not_a_predicate(line, col))?;
    let need = |k: &str| get(fields, k).ok_or_else(|| HelixError::new(format!("a `{kind}` predicate has no `{k}`"), line, col));
    let name_of = |k: &str| match need(k)? {
        Value::Str(s) => Ok(s.to_string()),
        other => Err(HelixError::new(format!("a `{kind}` predicate's `{k}` must be text, got {}", other.type_name()), line, col)),
    };
    Ok(match kind {
        "col" => {
            let name = name_of("name")?;
            if columns.contains(&name) {
                ColExpr::Col(name)
            } else {
                return Err(HelixError::new(format!("no column named `{name}`"), line, col)
                    .hint(format!("available columns: {}", columns.join(", "))));
            }
        }
        "lit" => {
            let v = need("value")?;
            // A leaf that holds a predicate — `p and @x > 1` wrote `p`, a name bound to
            // one, as a leaf — is that predicate.
            if is_predicate(v) {
                return from_value(v, columns, line, col);
            }
            validate_scalar(v, line, col)?;
            ColExpr::Lit(v.clone())
        }
        "bin" => {
            let sym = name_of("op")?;
            let op = binop_from_symbol(&sym)
                .ok_or_else(|| HelixError::new(format!("`{sym}` is not an operator a predicate can hold"), line, col))?;
            ColExpr::Binary(op, Box::new(from_value(need("left")?, columns, line, col)?), Box::new(from_value(need("right")?, columns, line, col)?))
        }
        "not" => ColExpr::Unary(UnOp::Not, Box::new(from_value(need("expr")?, columns, line, col)?)),
        "neg" => ColExpr::Unary(UnOp::Neg, Box::new(from_value(need("expr")?, columns, line, col)?)),
        "is_missing" => ColExpr::IsMissing(Box::new(from_value(need("expr")?, columns, line, col)?)),
        "is_nan" | "is_finite" => {
            ColExpr::FloatPred(float_pred_kind(kind).expect("a kind of this table"), Box::new(from_value(need("expr")?, columns, line, col)?))
        }
        "str" => {
            let name = name_of("name")?;
            let f = str_fn(&name).ok_or_else(|| HelixError::new(format!("`{name}` is not a String test a predicate can hold"), line, col))?;
            let expr = from_value(need("expr")?, columns, line, col)?;
            let mut vals = Vec::new();
            if let Value::Array(a) = need("args")? {
                for x in a.iter_values() {
                    validate_scalar(&x, line, col)?;
                    vals.push(x);
                }
            }
            ColExpr::StrMethod(f, Box::new(expr), vals)
        }
        _ => unreachable!("kind_of admits only the kinds of the table"),
    })
}

fn not_a_predicate(line: usize, col: usize) -> HelixError {
    HelixError::new("a frame verb was handed a record that is not a predicate", line, col).hint(
        "a predicate is what a column expression is as a value — `p = @age > 30` — a record whose `kind` is one of col, lit, bin, not, neg, is_missing, is_nan, is_finite, str.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> Vec<Stmt> {
        let toks = crate::lexer::lex(src).unwrap_or_else(|e| panic!("{}", e.message));
        crate::parser::parse(toks).unwrap_or_else(|e| panic!("{}", e.message))
    }
    fn value_of(stmt: &Stmt) -> &Expr {
        match stmt {
            Stmt::Assign { value, .. } | Stmt::Expr(value) => value,
            other => panic!("{other:?}"),
        }
    }
    fn field_of<'a>(e: &'a Expr, k: &str) -> &'a Expr {
        match e {
            Expr::Record(fs) => &fs.iter().find(|(n, _)| n == k).unwrap_or_else(|| panic!("no `{k}` in {e:?}")).1,
            other => panic!("not a record: {other:?}"),
        }
    }
    fn is_text(e: &Expr, t: &str) -> bool {
        matches!(e, Expr::Str(s) if s == t)
    }

    /// Every node of the frame's grammar has its record, and a leaf that is a value is a
    /// `lit` holding the expression as written.
    #[test]
    fn a_column_expression_outside_a_frame_verb_is_its_record() {
        let s = parsed("lo = 3\np = @age > lo and not @name.starts_with(\"x\") or @v.is_missing()\nq = -@a + 1\nr = is_nan(@f)");
        let p = value_of(&s[1]);
        assert!(is_text(field_of(p, "kind"), "bin") && is_text(field_of(p, "op"), "or"), "{p:?}");
        let and = field_of(p, "left");
        assert!(is_text(field_of(and, "op"), "and"), "{and:?}");
        let gt = field_of(and, "left");
        assert!(is_text(field_of(gt, "op"), ">"), "{gt:?}");
        assert!(is_text(field_of(field_of(gt, "left"), "kind"), "col") && is_text(field_of(field_of(gt, "left"), "name"), "age"));
        let lit = field_of(gt, "right");
        assert!(is_text(field_of(lit, "kind"), "lit") && matches!(field_of(lit, "value"), Expr::Ident { name, .. } if name == "lo"), "{lit:?}");
        let not = field_of(and, "right");
        assert!(is_text(field_of(not, "kind"), "not"));
        let sw = field_of(not, "expr");
        assert!(is_text(field_of(sw, "kind"), "str") && is_text(field_of(sw, "name"), "starts_with"), "{sw:?}");
        assert!(matches!(field_of(sw, "args"), Expr::Array(xs) if xs.len() == 1 && is_text(&xs[0], "x")));
        let im = field_of(p, "right");
        assert!(is_text(field_of(im, "kind"), "is_missing"));
        let q = value_of(&s[2]);
        assert!(is_text(field_of(q, "op"), "+") && is_text(field_of(field_of(q, "left"), "kind"), "neg"), "{q:?}");
        let r = value_of(&s[3]);
        assert!(is_text(field_of(r, "kind"), "is_nan") && is_text(field_of(field_of(r, "expr"), "name"), "f"), "{r:?}");
    }

    /// A frame verb's argument stays the frame's, wherever it is; a value made of a verb's
    /// result is a value; a column expression inside a value leaf is a value in turn.
    #[test]
    fn a_frame_verbs_argument_is_left_to_the_frame() {
        let s = parsed("D = dataframe({age: [1]})\nn = D.where(@age > 1).count() > 0\nw = [D.select(@age)]\nz = @a > f(@b)");
        let n = value_of(&s[1]);
        let Expr::Binary { left, .. } = n else { panic!("{n:?}") };
        let Expr::Method { recv, .. } = &**left else { panic!("{left:?}") };
        let Expr::Method { args, name, .. } = &**recv else { panic!("{recv:?}") };
        assert_eq!(name, "where");
        assert!(matches!(&args[0], Expr::Binary { left, .. } if matches!(**left, Expr::Column { .. })), "{args:?}");
        let Expr::Array(xs) = value_of(&s[2]) else { panic!() };
        let Expr::Method { args, .. } = &xs[0] else { panic!("{xs:?}") };
        assert!(matches!(args[0], Expr::Column { .. }));
        let z = value_of(&s[3]);
        let call = field_of(field_of(z, "right"), "value");
        let Expr::Call { args, .. } = call else { panic!("{call:?}") };
        assert!(is_text(field_of(&args[0], "kind"), "col"), "{args:?}");
    }

    /// A predicate value reads back as the expression it describes, column and all; a
    /// record that is not one is refused with the frame's own words.
    #[test]
    fn a_predicate_value_reads_back_as_the_frames_expression() {
        let mut interp = crate::interp::Interp::new();
        let stmts = parsed("p = @age > 30 and @name.starts_with(\"A\")\nq = {kind: \"col\", name: \"zzz\"}\nr = {a: 1}");
        interp.run(&stmts).unwrap_or_else(|e| panic!("{}", e.message));
        let cols = vec!["age".to_string(), "name".to_string()];
        let p = interp.global_value("p").expect("p");
        assert!(is_predicate(p));
        let e = from_value(p, &cols, 1, 1).unwrap_or_else(|e| panic!("{}", e.message));
        assert!(matches!(e, ColExpr::Binary(BinOp::And, _, _)), "{e:?}");
        let q = interp.global_value("q").expect("q");
        assert!(from_value(q, &cols, 1, 1).unwrap_err().message.contains("no column named `zzz`"));
        let r = interp.global_value("r").expect("r");
        assert!(!is_predicate(r));
    }
}
