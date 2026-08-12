//! The suggester's contract. These are the errors a newcomer hits in their first
//! five minutes, so every one of them is pinned by *exact text*.

use super::{Site, hint};
use crate::error::{edit_distance, typo_distance};

fn value(name: &str) -> Option<String> {
    hint(name, Site::Value, &[])
}
fn func(name: &str) -> Option<String> {
    hint(name, Site::Function, &[])
}
fn method(name: &str, methods: &[&str]) -> Option<String> {
    hint(name, Site::Method, methods)
}

/// The three most common foreign spellings on earth. `True` used to be told to
/// "assign it first"; `nil` was told it meant `pi`; `NA` was told it meant `e`.
#[test]
fn foreign_literals_get_the_helix_word() {
    for n in ["True", "TRUE", "true"] {
        assert_eq!(value(n).as_deref(), Some("did you mean `true`?"), "{n}");
    }
    for n in ["False", "FALSE"] {
        assert_eq!(value(n).as_deref(), Some("did you mean `false`?"), "{n}");
    }
    for n in ["None", "null", "NULL", "nil", "NA", "na", "undefined", "Nothing"] {
        assert_eq!(value(n).as_deref(), Some("did you mean `missing`?"), "{n}");
    }
}

/// `Inf` -> `inf` worked before this change only because `inf` is a *bound constant*
/// and so happened to be within two edits. It must keep working now that the match is
/// case-insensitive by construction. (`inf`/`pi`/`e` reach the suggester through
/// `extra`, exactly as the checker's env and the interpreter's globals hand them over
/// — this is the one thing the empty-`extra` helpers above cannot stand in for.)
#[test]
fn case_only_differences_still_resolve() {
    let consts = &["e", "inf", "pi", "tau"];
    assert_eq!(
        hint("Inf", Site::Value, consts).as_deref(),
        Some("did you mean `inf`?")
    );
    assert_eq!(
        hint("PI", Site::Value, consts).as_deref(),
        Some("did you mean `pi`?")
    );
    assert_eq!(func("PRINT").as_deref(), Some("did you mean `print`?"));
}

/// Every print-alike lands on `print` by name, not by edit distance — `cat` used to
/// be told it meant `cbrt` and `disp` that it meant `dict`.
#[test]
fn print_aliases() {
    for n in ["printf", "println", "cat", "disp", "console"] {
        assert_eq!(func(n).as_deref(), Some("did you mean `print`?"), "{n}");
    }
}

#[test]
fn conversion_aliases() {
    assert_eq!(func("int").as_deref(), Some("did you mean `to_int`?"));
    assert_eq!(func("float").as_deref(), Some("did you mean `to_float`?"));
    // There is no `to_str`; Helix interpolates. Say that rather than name a builtin
    // that does not exist.
    assert!(func("str").unwrap().contains("interpolating"));
    assert_eq!(func("len").as_deref(), Some("`len` is a method: `xs.length()`."));
    // `list` is one edit from the Array method `last`; without the alias a Python
    // user calling `list(xs)` was told to try `xs.last()`.
    assert_eq!(func("list").as_deref(), Some("did you mean `to_array`?"));
    assert_eq!(func("sorted").as_deref(), Some("`sorted` is a method: `xs.sort()`."));
    // At a method site the same alias is phrased as a method.
    assert_eq!(method("len", &["length"]).as_deref(), Some("did you mean `length`?"));
}

/// Requirement: cross the namespace instead of guessing a neighbour. `sum` used to
/// be told it meant `sin` — a wrong answer a scientist could believe.
#[test]
fn names_on_the_other_side_are_named_as_such() {
    assert_eq!(func("sum").as_deref(), Some("`sum` is a method: `xs.sum()`."));
    assert_eq!(func("upper").as_deref(), Some("`upper` is a method: `s.upper()`."));
    assert_eq!(func("sort").as_deref(), Some("`sort` is a method: `xs.sort()`."));
    assert_eq!(value("mean").as_deref(), Some("`mean` is a method: `xs.mean()`."));
    // …and the other direction: a function reached through a method call.
    assert_eq!(method("abs", &[]).as_deref(), Some("`abs` is a function: `abs(x)`."));
    assert_eq!(method("sqrt", &[]).as_deref(), Some("`sqrt` is a function: `sqrt(x)`."));
}

/// The heart of it: **no suggestion beyond one edit**, and no one-edit suggestion
/// inside a three-letter word. Every name here got a confident wrong answer before.
#[test]
fn no_suggestion_beyond_one_edit() {
    // Two edits away from their old answers.
    assert_eq!(value("qqq"), None);
    assert_eq!(func("blorp"), None);
    assert_eq!(value("xyzzy"), None);
    // A single edit, but inside a three-letter word — a different word, not a typo.
    // (`odd` used to be answered with `ord`.)
    assert_eq!(func("odd"), None);
    assert_eq!(func("ows"), None);
    assert_eq!(value("zpi"), None);
}

/// Distances the *rule* must accept and reject, independent of any candidate set.
#[test]
fn typo_rule() {
    // Transposition is one edit (`maen` for `mean` is the commonest typo there is).
    assert_eq!(edit_distance("maen", "mean"), 1);
    assert_eq!(edit_distance("naem", "name"), 1);
    assert_eq!(typo_distance("maen", "mean"), Some(1));
    assert_eq!(typo_distance("velociti", "velocity"), Some(1));
    assert_eq!(typo_distance("Intt", "Int"), Some(1));
    // Case alone is free.
    assert_eq!(typo_distance("Inf", "inf"), Some(0));
    // Rejected: two edits, or one edit in a short word.
    assert_eq!(typo_distance("nil", "pi"), None);
    assert_eq!(typo_distance("NA", "e"), None);
    assert_eq!(typo_distance("sum", "sin"), None);
    assert_eq!(typo_distance("cat", "cbrt"), None);
    assert_eq!(typo_distance("disp", "dict"), None);
    assert_eq!(typo_distance("odd", "ord"), None);
    assert_eq!(typo_distance("str", "sqrt"), None);
    assert_eq!(typo_distance("len", "ln"), None);
    assert_eq!(typo_distance("int", "print"), None);
}

/// Real typos of real names still get answered — the point is precision, not silence.
#[test]
fn genuine_typos_still_answered() {
    assert_eq!(func("prnit").as_deref(), Some("did you mean `print`?"));
    assert_eq!(func("linspcae").as_deref(), Some("did you mean `linspace`?"));
    assert_eq!(
        hint("velociti", Site::Function, &["velocity"]).as_deref(),
        Some("did you mean `velocity`?")
    );
    assert_eq!(
        hint("totl", Site::Value, &["total"]).as_deref(),
        Some("did you mean `total`?")
    );
    assert_eq!(method("maen", &["mean", "median"]).as_deref(), Some("did you mean `mean`?"));
}

/// A near match *at* a method site is already after the dot, so it is phrased as a
/// plain method name — not re-decorated with a receiver.
#[test]
fn method_site_near_matches_are_phrased_plainly() {
    let array = crate::registry::methods_of(crate::registry::ARRAY_METHODS);
    assert_eq!(method("maen", &array).as_deref(), Some("did you mean `mean`?"));
    let string = crate::registry::methods_of(crate::registry::STRING_METHODS);
    assert_eq!(method("uppr", &string).as_deref(), Some("did you mean `upper`?"));
    // A bare-name alias only applies at a method site when the receiver really has
    // it: `"1".int()` is `to_int` (a String method); `x.println()` is not `print`.
    assert_eq!(method("int", &string).as_deref(), Some("did you mean `to_int`?"));
    assert_eq!(method("println", &string), None);
    assert!(method("to_str", &string).unwrap().contains("interpolating"));
}

/// A binding the user wrote outranks a same-distance method name — `count` the
/// variable is a better answer than `xs.count()` when the user has a `count`.
#[test]
fn bindings_outrank_methods_at_the_same_distance() {
    assert_eq!(
        hint("cont", Site::Value, &["count"]).as_deref(),
        Some("did you mean `count`?")
    );
    // With no such binding, the method is still worth naming — but as a GUESS, because
    // `cont` is not a method and saying it is would be false. That distinction is not
    // pedantry: the same code path answered `read("f.csv")` with "`read` is a method:
    // `df.head()`.", and neither half of that sentence was true. "`X` is a method" is
    // reserved for rule 2, where the name really is one.
    assert_eq!(
        value("cont").as_deref(),
        Some("did you mean the method `count`? e.g. `xs.count()`.")
    );
}

/// A NEAR MATCH IS PHRASED AS A GUESS, ALWAYS — including when it lands in the other
/// namespace. Rule 2's "`sum` is a method: `xs.sum()`" is a statement of FACT and is only
/// available when the name really is a method; rule 3 has no such licence.
///
/// Found by an adversarial sweep, which is the only way it could be: the change's own review
/// enumerated distance-1 pairs WITHIN the name universe and judged them legitimate
/// neighbours. It never asked which foreign spellings reach those pairs, and the answer
/// includes `read`, which an R or Python user types constantly.
#[test]
fn a_near_match_never_claims_the_users_name_is_a_method() {
    for (typed, must_not_say) in [
        ("read", "`read` is a method"),
        ("hash", "`hash` is a method"),
        ("vars", "`vars` is a method"),
        ("lenght", "`lenght` is a method"),
    ] {
        let h = value(typed).unwrap_or_default();
        assert!(
            !h.contains(must_not_say),
            "`{typed}` is not a method, but the hint says it is: {h}"
        );
        // It may still guess — a guess that says so is useful; a false statement is not.
        assert!(
            h.is_empty() || h.starts_with("did you mean"),
            "a near match must be phrased as a question: {h}"
        );
    }
}

/// A method site never reaches for another type's methods: `"ab".mean()` must not
/// be told about the Array method, because it would not work.
#[test]
fn method_sites_stay_inside_their_own_table() {
    let string_methods = crate::registry::methods_of(crate::registry::STRING_METHODS);
    // `median` is an Array method; a String receiver gets nothing from it.
    assert_eq!(method("mediam", &string_methods), None);
    // …but the same name against the Array table is answered.
    let array_methods = crate::registry::methods_of(crate::registry::ARRAY_METHODS);
    assert_eq!(method("mediam", &array_methods).as_deref(), Some("did you mean `median`?"));
}

/// The answer must not depend on the order candidates were collected — several
/// callers hand over `FxHashMap` keys, and the three engines must agree byte for byte.
#[test]
fn candidate_order_does_not_change_the_answer() {
    let a = hint("mena", Site::Value, &["mend", "meno", "mean"]);
    let b = hint("mena", Site::Value, &["mean", "meno", "mend"]);
    let c = hint("mena", Site::Value, &["meno", "mean", "mend"]);
    assert_eq!(a, b);
    assert_eq!(b, c);
    assert!(a.is_some());
}
