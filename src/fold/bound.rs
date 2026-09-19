//! What a record BUILT AT RUN TIME is known to hold (ADR 0051; field build §1.62).
//!
//! `P = People.on(db)` — bind the connection once, call verbs on the result — is the shape of
//! every configure-once API, and the load-time sandbox can never hold `P`: `db` is I/O. So
//! `P.sql(spec)` stayed a dynamic method call, nothing about `spec`'s shape was used, and
//! every verb on the bound model paid four to five times what the unbound model paid.
//!
//! This module answers one question and changes nothing: for a top-level name bound to such
//! a record, WHICH CLOSURE is field `f` certain to hold? It evaluates the initializer
//! ABSTRACTLY — reading values the sandbox already holds, building records symbolically,
//! following calls to the program's own functions and to closures it knows, and treating
//! everything else as unknown. `{...m, target: t, rows: (s) => …}` with `m` a held model is
//! a record whose `sql` is exactly the closure `m` holds there, whatever `t` turns out to be;
//! its `target` and `rows` are unknown, and a field a later part overrides is unknown too.
//!
//! IT REWRITES NO VALUE. A spread copies the closure ITSELF, and Helix compares functions by
//! identity: `M.sql == P.sql` is `true`, and must stay so. The first cut of this wrote `P`
//! out as a literal whose closure fields were new top-level functions — an optimization
//! changing what `==` answers. Only the CALL `P.sql(spec)` is pointed at the devirtualized
//! closure, exactly as `M.sql(spec)` already is; the record `P` stays what the program builds.
//!
//! WHY THE ANSWER IS CERTAIN. Everything followed here is deterministic construction: a held
//! global is immutable and pure, so it is the value the program computes; records are values
//! and cannot be mutated; a call is followed only to a function the program declares once and
//! never rebinds, with exactly its arity, and a method only through a field holding a known
//! closure whose name no record method owns (ADR 0045: method, then field). Effects or a raise
//! on the way do not matter — if the initializer raises, the name is never bound and no call
//! through it runs. Anything conditional, defaulted, shadowed or opaque is `Unknown`.

use std::rc::Rc;

use crate::ast::{Expr, RecordPart};
use crate::value::{FuncVal, Value};

/// How far calls are followed, and how many nodes are read, before the answer is `Unknown`.
const MAX_DEPTH: usize = 8;
const FUEL: usize = 4_096;

/// What is known of a value before the program runs.
#[derive(Clone)]
pub(super) enum Known {
    /// A value the sandbox holds — exactly what the program computes.
    Value(Value),
    /// A record whose field NAMES are certain, each field known or not.
    Record(Vec<(String, Known)>),
    Unknown,
}

/// A top-level value the sandbox holds, bound on demand.
pub(super) type HeldValue<'a> = dyn FnMut(&str) -> Option<Value> + 'a;
/// One of the program's own functions: its parameter names and its body.
pub(super) type ProgramFn<'a> = dyn Fn(&str) -> Option<(Vec<String>, Expr)> + 'a;

/// What the evaluation reads from.
pub(super) struct Reads<'a> {
    global: &'a mut HeldValue<'a>,
    func: &'a ProgramFn<'a>,
    fuel: usize,
}

impl<'a> Reads<'a> {
    pub(super) fn new(global: &'a mut HeldValue<'a>, func: &'a ProgramFn<'a>) -> Self {
        Reads { global, func, fuel: FUEL }
    }

    /// The closure-valued fields the record `init` builds is CERTAIN to hold, by name —
    /// empty when `init` is not known to build a record, or none of its fields is one.
    pub(super) fn closures_of(&mut self, init: &Expr) -> Vec<(String, Rc<FuncVal>)> {
        let fields = match self.eval(init, &mut Vec::new(), 0) {
            Known::Record(fs) => fs,
            Known::Value(Value::Record(fs)) => fs.iter().map(|(s, v)| (s.as_str().to_string(), Known::Value(v.clone()))).collect(),
            _ => return Vec::new(),
        };
        fields
            .into_iter()
            .filter_map(|(k, v)| match v {
                Known::Value(Value::Function(fv)) => Some((k, fv)),
                _ => None,
            })
            .collect()
    }

    fn eval(&mut self, e: &Expr, env: &mut Vec<(String, Known)>, depth: usize) -> Known {
        if self.fuel == 0 || depth > MAX_DEPTH {
            return Known::Unknown;
        }
        self.fuel -= 1;
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {
                crate::interp::const_default_value(e).map(Known::Value).unwrap_or(Known::Unknown)
            }
            Expr::Ident { name, .. } => match env.iter().rev().find(|(n, _)| n == name) {
                Some((_, k)) => k.clone(),
                None => (self.global)(name).map(Known::Value).unwrap_or(Known::Unknown),
            },
            Expr::Record(fields) => Known::Record(fields.iter().map(|(k, v)| (k.clone(), self.eval(v, env, depth))).collect()),
            // Parts in written order, a later one winning IN PLACE — `spread_into`'s rule at
            // run time. A spread of anything not known to be a record makes every field
            // uncertain: it may carry any name.
            Expr::RecordUpdate { parts, .. } => {
                let mut out: Vec<(String, Known)> = Vec::new();
                let put = |out: &mut Vec<(String, Known)>, k: String, v: Known| match out.iter_mut().find(|(f, _)| *f == k) {
                    Some(slot) => slot.1 = v,
                    None => out.push((k, v)),
                };
                for p in parts {
                    match p {
                        RecordPart::Spread(src) => match self.eval(src, env, depth) {
                            Known::Value(Value::Record(fs)) => {
                                for (s, v) in fs.iter() {
                                    put(&mut out, s.as_str().to_string(), Known::Value(v.clone()));
                                }
                            }
                            Known::Record(fs) => {
                                for (k, v) in fs {
                                    put(&mut out, k, v);
                                }
                            }
                            _ => return Known::Unknown,
                        },
                        RecordPart::Field(k, v) => {
                            let v = self.eval(v, env, depth);
                            put(&mut out, k.clone(), v);
                        }
                    }
                }
                Known::Record(out)
            }
            Expr::Let { bindings, body, .. } => {
                let mark = env.len();
                for (n, v) in bindings {
                    let k = self.eval(v, env, depth);
                    env.push((n.clone(), k));
                }
                let r = self.eval(body, env, depth);
                env.truncate(mark);
                r
            }
            Expr::Field { recv, name, .. } => match self.eval(recv, env, depth) {
                Known::Value(Value::Record(fs)) => {
                    fs.iter().find(|(s, _)| s.as_str() == name).map(|(_, v)| Known::Value(v.clone())).unwrap_or(Known::Unknown)
                }
                Known::Record(fs) => fs.into_iter().find(|(k, _)| k == name).map(|(_, v)| v).unwrap_or(Known::Unknown),
                _ => Known::Unknown,
            },
            // One of the program's own functions, by name, with exactly its arity. A local of
            // the name would be a function VALUE, which this does not follow.
            Expr::Call { name, args, .. } if !env.iter().any(|(n, _)| n == name) => {
                let Some((params, body)) = (self.func)(name) else { return Known::Unknown };
                if params.len() != args.len() {
                    return Known::Unknown;
                }
                let mut callee: Vec<(String, Known)> = Vec::with_capacity(params.len());
                for (p, a) in params.iter().zip(args.iter()) {
                    let k = self.eval(a, env, depth);
                    callee.push((p.clone(), k));
                }
                self.eval(&body, &mut callee, depth + 1)
            }
            // A closure held in a record's field, applied: its captured values under its
            // parameters. A name a record's own methods own is never a field call.
            Expr::Method { recv, name, args, named, .. } if named.is_empty() && !crate::registry::type_owns_method("Record", name) => {
                let field = match self.eval(recv, env, depth) {
                    Known::Value(Value::Record(fs)) => fs.iter().find(|(s, _)| s.as_str() == name).map(|(_, v)| Known::Value(v.clone())),
                    Known::Record(fs) => fs.into_iter().find(|(k, _)| k == name).map(|(_, v)| v),
                    _ => None,
                };
                let Some(Known::Value(Value::Function(fv))) = field else { return Known::Unknown };
                if fv.params.len() != args.len() {
                    return Known::Unknown;
                }
                let mut callee: Vec<(String, Known)> = fv.captured.iter().map(|(n, v)| (n.clone(), Known::Value(v.clone()))).collect();
                for (p, a) in fv.params.iter().zip(args.iter()) {
                    let k = self.eval(a, env, depth);
                    callee.push((p.clone(), k));
                }
                self.eval(&fv.body, &mut callee, depth + 1)
            }
            _ => Known::Unknown,
        }
    }
}
