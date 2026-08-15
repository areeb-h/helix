//! The one place that answers **"did you mean …?"**.
//!
//! Every `is not defined` / `is not a known function` / `has no method` error routes
//! its hint through [`hint`], so the advice a newcomer gets is the same whichever
//! engine produced the error and whichever of the eleven error constructors fired.
//!
//! Three rules, in order:
//!
//! 1. **Foreign aliases.** The spellings people arrive with — `True`, `None`, `nil`,
//!    `NA`, `len`, `println` — are not typos, they are another language's word for a
//!    thing Helix has. Guessing a Helix name by edit distance for those is how
//!    `nil` became `pi`. They get an explicit table.
//! 2. **Cross the namespace.** If the name exists on the *other* side of the
//!    function/method divide, say where it lives instead of hunting for a neighbour:
//!    `sum([1,2,3])` → ``sum` is a method: `xs.sum()``, and `(-1).abs()` →
//!    ``abs` is a function: `abs(x)``.
//! 3. **Near match**, over builtins + bindings + *every* type's method names + the
//!    keyword literals `true`/`false`/`missing` — case-insensitively, and only
//!    within one edit ([`crate::error::typo_distance`]). A wrong suggestion is worse
//!    than silence: a scientist who is told `sum` means `sin` may believe it.
//!
//! There is deliberately **no fallback**. The old one — ``assign it first, e.g.
//! `None = ...`.`` — was never good advice; it told people to define a variable
//! called `None` instead of telling them Helix spells it `missing`.

use crate::error::typo_distance;
use crate::registry;

/// Where the unknown name appeared. Decides which candidates are in scope and how
/// a match is phrased.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// A bare name used as a value: `x = True`.
    Value,
    /// A name in call position: `sum([1, 2, 3])`.
    Function,
    /// A method on a receiver: `(-1).abs()`. `extra` is that receiver's method table.
    Method,
}

/// The keyword literals. They are `Tok::` variants, not bindings, so they are in no
/// environment and no registry — which is exactly why `Inf` → `inf` used to work
/// (`inf` is a bound constant) and `True` → `true` did not.
pub const KEYWORDS: &[&str] = &["true", "false", "missing"];

/// What a foreign spelling should become.
enum Target {
    /// A bare name: a keyword literal, a builtin, a constant.
    Name(&'static str),
    /// A method — rendered `xs.length()` away from a method call site.
    Method(&'static str),
    /// Free-form advice, for the ones with no single-name answer.
    Text(&'static str),
}

/// Spellings from other languages, matched case-insensitively. These are *not*
/// typos, so no edit-distance rule applies to them.
const ALIASES: &[(&str, Target)] = &[
    // Booleans: Python/R shout them, Helix does not.
    ("true", Target::Name("true")),
    ("false", Target::Name("false")),
    // Every language's null. Helix has exactly one (ADR-0001).
    ("none", Target::Name("missing")),
    ("null", Target::Name("missing")),
    ("nil", Target::Name("missing")),
    ("na", Target::Name("missing")),
    ("undefined", Target::Name("missing")),
    ("nothing", Target::Name("missing")),
    // Conversions and length. `list` is here because it is one edit from the Array
    // method `last` — a Python user writing `list(xs)` was told to try `xs.last()`,
    // which is the exact failure this table exists to prevent.
    ("list", Target::Name("to_array")),
    ("array", Target::Name("to_array")),
    ("len", Target::Method("length")),
    ("sorted", Target::Method("sort")),
    ("int", Target::Name("to_int")),
    ("float", Target::Name("to_float")),
    // `to_str`/`str()` do NOT exist — Helix converts to text by interpolating.
    ("str", Target::Text(
        "Helix converts to text by interpolating: `\"{x}\"` (or `x.to_json()` for data).",
    )),
    ("to_str", Target::Text(
        "Helix converts to text by interpolating: `\"{x}\"` (or `x.to_json()` for data).",
    )),
    ("tostring", Target::Text(
        "Helix converts to text by interpolating: `\"{x}\"` (or `x.to_json()` for data).",
    )),
    // Printing.
    ("printf", Target::Name("print")),
    ("println", Target::Name("print")),
    ("cat", Target::Name("print")),
    ("disp", Target::Name("print")),
    // `console.log(...)` parses as a method on the bare name `console`.
    ("console", Target::Name("print")),
    ("puts", Target::Name("print")),
    ("fprintf", Target::Name("print")),
    ("sprintf", Target::Text(
        "Helix formats by interpolating, with an optional spec: `\"{x:.2f}\"`.",
    )),
    // The rest of the foreign-builtin long tail, from an adversarial sweep of 1438 programs
    // written the way a newcomer would write them. Each of these was reaching NO help at all
    // — the edit-distance cap correctly refuses to guess `paste` -> `parse`, and refusing is
    // right, but an exact answer is better than either.
    ("parseint", Target::Name("to_int")),
    ("parsefloat", Target::Name("to_float")),
    ("string", Target::Text(
        "Helix converts to text by interpolating: `\"{x}\"` (or `x.to_json()` for data).",
    )),
    ("toupper", Target::Method("upper")),
    ("tolower", Target::Method("lower")),
    ("nchar", Target::Method("length")),
    ("numel", Target::Method("length")),
    ("size", Target::Method("length")),
    ("count", Target::Method("count")),
    ("prefix_sum", Target::Text(
        "a prefix sum is `xs.cumsum()`; the general running reduction is `xs.scan(init, f)`.",
    )),
    ("cumulative_sum", Target::Text(
        "a prefix sum is `xs.cumsum()`; the general running reduction is `xs.scan(init, f)`.",
    )),
    ("seq", Target::Text(
        "a sequence is `range(start, stop)` or the literal `(0..n)`; step with `range(a, b, step)`.",
    )),
    ("paste", Target::Text(
        "join text by interpolating — `\"{a}{b}\"` — or `xs.join(\", \")` for a list.",
    )),
    ("paste0", Target::Text(
        "join text by interpolating — `\"{a}{b}\"` — or `xs.join(\"\")` for a list.",
    )),
    ("nan", Target::Text(
        "Helix has no `NaN` literal: produce one with `sqrt(0.0 - 1.0)`, and test with `is_nan(x)`.",
    )),
    ("inf", Target::Name("inf")),
    // Namespace objects. `Math.abs(-1)`, `JSON.parse(s)`, `fmt.Println(1)` and `np.array(x)`
    // all parse as a method on a bare name that does not exist, so the name is what fails.
    ("math", Target::Text(
        "Helix's math is free functions, not a namespace: `sqrt(x)`, `abs(x)`, `exp(x)`, `log(x)`.",
    )),
    ("json", Target::Text(
        "JSON is `parse_json(text)` and `value.to_json()` — there is no `JSON` object.",
    )),
    ("fmt", Target::Name("print")),
    ("np", Target::Text(
        "there is no NumPy to import — arrays and `tensor(...)` are built in, and `map`/`reduce` are methods.",
    )),
    ("pd", Target::Text(
        "there is no pandas to import — `read_csv(path)` returns a DataFrame directly.",
    )),
    ("dplyr", Target::Text(
        "DataFrame verbs are methods: `df.where(@a > 1).select(@b).group(@c).mean(@d)`.",
    )),
];

/// A natural receiver name per type, so a cross-namespace hint reads like code a
/// user would write (`xs.sum()`, `s.upper()`, `df.select(...)`).
pub(crate) fn receiver_for(type_name: &str) -> &'static str {
    match type_name {
        "Array" => "xs",
        "String" => "s",
        "Dna" => "seq",
        "Tensor" => "t",
        "DataFrame" => "df",
        "GroupBy" => "g",
        "Dict" => "d",
        "Net" => "net",
        "Record" => "rec",
        _ => "x",
    }
}

/// The receiver name to use when suggesting `name` as a method, or `None` if no
/// type has such a method. Array wins ties because it is the common case.
fn method_receiver(name: &str) -> Option<&'static str> {
    for (ty, table) in registry::type_method_tables() {
        if table.contains(&name) {
            return Some(receiver_for(ty));
        }
    }
    registry::UNIVERSAL_METHODS.contains(&name).then_some("x")
}

/// Whether an alias is worth offering at this site. Away from a method call every
/// alias applies. *At* one, a bare-name target only applies if the receiver really
/// has that method (`"1".int()` → `to_int`, a String method); otherwise the answer
/// would name a function as though it were a method (`x.println()` → `print`), and
/// the cross-namespace rule below says that better.
fn applies_at(target: &Target, site: Site, extra: &[&str]) -> bool {
    match (site, target) {
        (Site::Method, Target::Name(n)) => extra.contains(n),
        _ => true,
    }
}

fn render(target: &Target, name: &str, site: Site) -> String {
    match target {
        Target::Name(n) => format!("did you mean `{n}`?"),
        Target::Method(m) => match site {
            Site::Method => format!("did you mean `{m}`?"),
            _ => {
                let recv = method_receiver(m).unwrap_or("xs");
                format!("`{name}` is a method: `{recv}.{m}()`.")
            }
        },
        Target::Text(t) => t.to_string(),
    }
}

/// Append `names`, sorted and deduped, to `out`. Sorting is what makes the chosen
/// suggestion independent of hash-map iteration order — the three engines must
/// render byte-identical errors, and `extra` arrives from an `FxHashMap`'s keys.
fn push_group<'a>(
    names: &mut Vec<&'a str>,
    as_methods: bool,
    out: &mut Vec<(&'a str, Option<&'static str>)>,
) {
    names.sort_unstable();
    names.dedup();
    for &n in names.iter() {
        out.push((n, if as_methods { method_receiver(n) } else { None }));
    }
}

/// Candidate names in scope at `site`, in tie-break priority order (earlier groups
/// win ties).
fn candidates<'a>(site: Site, extra: &[&'a str]) -> Vec<(&'a str, Option<&'static str>)> {
    let mut out: Vec<(&'a str, Option<&'static str>)> = Vec::new();

    // 1. Names the user themself put in scope (bindings, or user functions at a call
    //    site) — or, at a method site, the receiver's own method table. Never
    //    receiver-decorated: at a method site we are already after the dot, and at a
    //    value/call site these are bindings and functions, not methods.
    push_group(&mut extra.to_vec(), false, &mut out);

    if site == Site::Method {
        // A method site's near-match universe is that receiver's own methods only:
        // offering an Array method for a String receiver is a wrong answer, not a
        // near one. Crossing to the functions is handled ahead of this, in `hint`.
        return out;
    }

    // 2. The builtins.
    push_group(&mut registry::names().collect(), false, &mut out);

    // 3. The keyword literals — bindings-that-aren't, invisible to every env.
    if site == Site::Value {
        push_group(&mut KEYWORDS.to_vec(), false, &mut out);
    }

    // 4. Every type's methods, last: `xs.count()` is a worse answer than a binding
    //    named `count`, but a far better one than silence.
    push_group(
        &mut registry::type_method_tables()
            .iter()
            .flat_map(|(_, t)| t.iter().copied())
            .chain(registry::UNIVERSAL_METHODS.iter().copied())
            .collect(),
        true,
        &mut out,
    );

    out
}

/// The hint for an unknown `name` at `site`, or `None` when nothing is close enough
/// to be worth saying. `extra` is the context-dependent candidate set: bindings in
/// scope at a value site, user functions at a call site, the receiver's method table
/// at a method site.
pub fn hint(name: &str, site: Site, extra: &[&str]) -> Option<String> {
    // 1. A foreign spelling is not a typo — answer it exactly.
    let lower = name.to_lowercase();
    if let Some((_, target)) = ALIASES.iter().find(|(k, _)| *k == lower)
        && applies_at(target, site, extra)
    {
        return Some(render(target, name, site));
    }

    // 2. Cross the namespace before guessing a neighbour.
    match site {
        Site::Method => {
            if registry::lookup(name).is_some() {
                return Some(format!("`{name}` is a function: `{name}(x)`."));
            }
        }
        Site::Value | Site::Function => {
            if let Some(recv) = method_receiver(name) {
                return Some(format!("`{name}` is a method: `{recv}.{name}()`."));
            }
        }
    }

    // 3. Near match — within one edit, case-insensitively.
    let mut best: Option<(usize, &str, Option<&'static str>)> = None;
    for (cand, recv) in candidates(site, extra) {
        let Some(d) = typo_distance(name, cand) else { continue };
        if best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, cand, recv));
        }
    }
    best.map(|(_, cand, recv)| match recv {
        // A NEAR MATCH IS A GUESS, AND MUST BE PHRASED AS ONE. Rule 2's sentence —
        // "`sum` is a method: `xs.sum()`" — is a statement of fact about the user's own
        // name, and it is only true there because `sum` really is a method. Reusing it here
        // asserts something false the moment the near match lands in the other namespace:
        // `read("f.csv")` was answered ``read` is a method: `df.head()`.`, and `read` is not
        // a method, `head` is not what was meant, and both halves of the sentence are wrong.
        // (Also seen: `hash` -> `d.has()`, `vars` -> `xs.var()`.)
        //
        // A wrong suggestion is worse than silence — a scientist may believe it — so the
        // guess says it is guessing, and says where the guess lives.
        Some(r) => format!("did you mean the method `{cand}`? e.g. `{r}.{cand}()`."),
        None => format!("did you mean `{cand}`?"),
    })
}

#[cfg(test)]
mod tests;
