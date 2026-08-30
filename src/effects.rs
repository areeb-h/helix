//! The effect closure of a user function, over the call graph.
//!
//! Helix already classifies every BUILTIN along two independent axes, and both are held
//! exhaustively by drift guards:
//!
//! - **Authority** — [`crate::capability::effect_of`] / `method_effect_of`, the fs/net/process
//!   categories the sandbox gates. `no_ungated_effectful_builtins` walks `BUILTINS` and forces
//!   every effectful one to be either gated or in a justified `harmless` allowlist.
//! - **Reproducibility** — `BuiltinDef::pure` in the registry, which decides whether a result
//!   may be memoized.
//!
//! **The two axes are genuinely separate, and that is the point of this module.** `now()` and
//! `clock_monotonic()` are `pure: false` and yet hold no authority: they are in the harmless
//! allowlist precisely because reading a clock touches no file and no socket. A function built
//! from them is fully sandboxable and still not reproducible. Reporting only one axis would
//! hide exactly that case.
//!
//! What is missing is PROPAGATION, and that is all this module adds. It is not an effect
//! system: nothing here infers, unifies or annotates. It walks the call graph a program
//! already has and reports the classifications Helix already made, with the path that
//! introduced each one — because "this is not deterministic" is not actionable, and "not
//! deterministic: `now`, via report -> summarize" is.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Expr, Stmt};
use crate::capability::Effect;

/// What one function reaches.
#[derive(Debug, Clone, PartialEq)]
pub struct FnEffects {
    pub name: String,
    /// Authority categories reached, each with the call path that introduced it.
    pub effects: Vec<(Effect, Vec<String>)>,
    /// `None` when the function reaches nothing irreproducible; otherwise the name that
    /// makes it so and the path to it.
    pub nondeterministic: Option<(String, Vec<String>)>,
}

impl FnEffects {
    pub fn deterministic(&self) -> bool {
        self.nondeterministic.is_none()
    }
}

/// Every top-level function in a loaded program, with its effect closure.
///
/// Recursion terminates on a visited set rather than a depth limit: a cycle contributes
/// nothing new by definition, since effects are a union.
pub fn closure(stmts: &[Stmt]) -> Vec<FnEffects> {
    let mut bodies: BTreeMap<&str, &Expr> = BTreeMap::new();
    for s in stmts {
        if let Stmt::Func { name, body, .. } = s {
            bodies.insert(name.as_str(), body);
        }
    }
    bodies
        .keys()
        .map(|name| {
            let mut w = Walk { bodies: &bodies, seen: BTreeSet::new(), found: BTreeMap::new(), nd: None };
            w.visit(name, &mut Vec::new());
            let mut effects: Vec<(Effect, Vec<String>)> = w.found.into_iter().collect();
            effects.sort_by_key(|(e, _)| e.label());
            FnEffects { name: (*name).to_string(), effects, nondeterministic: w.nd }
        })
        .collect()
}

struct Walk<'a> {
    bodies: &'a BTreeMap<&'a str, &'a Expr>,
    seen: BTreeSet<String>,
    /// First path that reached each effect. FIRST, not shortest: the walk is deterministic,
    /// so this is stable, and a reader wants *a* witness rather than the best one.
    found: BTreeMap<Effect, Vec<String>>,
    nd: Option<(String, Vec<String>)>,
}

impl Walk<'_> {
    fn visit(&mut self, name: &str, path: &mut Vec<String>) {
        if !self.seen.insert(name.to_string()) {
            return;
        }
        path.push(name.to_string());
        if let Some(body) = self.bodies.get(name) {
            let calls = calls_in(body);
            for (callee, is_method) in calls {
                self.record(&callee, is_method, path);
                if !is_method && self.bodies.contains_key(callee.as_str()) {
                    self.visit(&callee, path);
                }
            }
        }
        path.pop();
    }

    fn record(&mut self, callee: &str, is_method: bool, path: &[String]) {
        let eff = if is_method {
            crate::capability::method_effect_of(callee)
        } else {
            crate::capability::effect_of(callee)
        };
        if eff.gated() {
            let mut p = path.to_vec();
            p.push(callee.to_string());
            self.found.entry(eff).or_insert(p);
        }
        // A METHOD'S PURITY IS NOT IN THE BUILTIN CATALOG, so only free calls answer the
        // reproducibility question here. Claiming determinism from an incomplete axis would
        // be worse than saying nothing, so an unknown name is treated as reproducible only
        // when the catalog actually says so.
        if !is_method
            && self.nd.is_none()
            && let Some(def) = crate::registry::lookup(callee)
            && !def.pure
            // Output is an effect, not an input: `print` cannot change what a function
            // COMPUTES, and calling a printing function irreproducible would flag almost
            // every program while telling no one anything.
            && !matches!(callee, "print" | "emit" | "write" | "elog")
        {
            let mut p = path.to_vec();
            p.push(callee.to_string());
            self.nd = Some((callee.to_string(), p));
        }
    }
}

/// Every call and method name appearing under `e`, paired with whether it was a method.
///
/// Uses the shared [`crate::visit::walk_expr`], whose exhaustive match means a new `Expr`
/// variant fails compilation there rather than being silently skipped here.
fn calls_in(e: &Expr) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    crate::visit::walk_expr(e, &mut |x| match x {
        Expr::Call { name, .. } => out.push((name.clone(), false)),
        Expr::Method { name, .. } => out.push((name.clone(), true)),
        _ => {}
    });
    out
}
