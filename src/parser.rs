//! Recursive-descent parser with precedence climbing.
//!
//! Grammar (informal):
//!   program    := { NL } [ stmt { NL+ stmt } ] { NL }
//!   stmt       := [ "mut" ] ident "=" expr        -- assignment
//!               | expr                             -- expression statement
//!   expr       := or
//!   or         := and   { "or"  and }
//!   and        := eq    { "and" eq }
//!   eq         := cmp   { ("==" | "!=") cmp }
//!   cmp        := term  { ("<" | ">" | "<=" | ">=") term }
//!   term       := factor{ ("+" | "-") factor }
//!   factor     := unary { ("*" | "/" | "%") unary }
//!   unary      := ("-" | "not") unary | postfix
//!   postfix    := primary { "." ident "(" args ")" | "[" expr "]" | "(" args ")" }
//!   primary    := int | float | str | "true" | "false" | ident
//!               | "(" expr ")" | "[" [ expr { "," expr } ] "]"

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, InterpPart, Stmt, TypeAnn, UnOp};
use crate::error::{suggest, HelixError};
use crate::token::{StrSeg, Tok, Token};

/// A user function's signature, captured at its definition so calls to it can be
/// desugared: named arguments reordered into position and omitted parameters filled
/// with their (literal) defaults. `defaults` is parallel to `params`.
#[derive(Clone)]
struct FnSig {
    params: Vec<String>,
    defaults: Vec<Option<Expr>>,
}

/// A parsed call's arguments: positional expressions and `name: value` named pairs.
type CallArgs = (Vec<Expr>, Vec<(String, Expr)>);

/// True for the literal-constant expressions allowed as a parameter default. The
/// default is inserted at each call site, so it must not reference anything (no
/// params, no globals) — a literal (optionally negated/notted) guarantees that.
fn is_const_default(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => true,
        Expr::Unary { expr, .. } => is_const_default(expr),
        _ => false,
    }
}

/// Lex + parse a single expression (used for `{expr}` interpolation fragments).
/// `depth` is the enclosing parser's current nesting depth, so a `{...}` hole that
/// itself contains an interpolated string keeps accumulating toward
/// `MAX_PARSE_DEPTH` rather than resetting - bounding nested-interpolation recursion.
/// `sigs` carries the enclosing program's function signatures (defined-so-far), so a call
/// inside the hole resolves named arguments and defaults exactly as it would outside — an
/// interpolated `"{greet(name, loud: true)}"` is the same call as a bare one.
/// Returns the hole's expression plus any `do {}` binding names it contained, so the
/// enclosing parser can fold them into its own list — otherwise an interpolated
/// `"{do { n = 1 … }}"` would be the one place the mut-global shadow check does not reach.
///
/// `imports` rides along for the same reason `sigs` does: an interpolation hole is a
/// fresh Parser, and every piece of enclosing-parser state it does NOT inherit is a
/// place where a rule holds outside a string but not inside one. That is not
/// hypothetical — the module-namespace guard on the comprehension desugars shipped in
/// v0.2.2 covering `print(mod.position(…))` but not `emit("{mod.position(…)}")`,
/// because this constructor filled `imports` with an empty set. The field report that
/// caught it noted the interpolated form is the idiomatic one, so the fix had missed
/// exactly where users hit it first.
fn parse_expression(
    src: &str,
    depth: usize,
    sigs: &HashMap<String, FnSig>,
    imports: &std::collections::HashSet<String>,
    fn_names: &std::collections::HashSet<String>,
) -> Result<(Expr, Vec<DoBinding>), HelixError> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        depth,
        fn_sigs: (*sigs).clone(),
        do_bindings: Vec::new(),
        imports: imports.clone(),
        selected_imports: std::collections::HashSet::new(),
        // Threaded for the same reason `imports` is, and recorded above: a hole's
        // parser that does not inherit an enclosing rule makes that rule hold outside
        // strings and not inside them, which is where people meet it first.
        fn_names: fn_names.clone(),
    };
    p.skip_newlines();
    let e = p.expr()?;
    p.skip_newlines();
    if !p.at_end() {
        let (l, c) = p.pos();
        // `{2,}` is the overwhelmingly common case here and it is not an interpolation
        // at all — it is a regex quantifier in an ordinary string, which Helix reads as
        // `{…}`. Naming the raw-string form turns a confusing parse error into the fix.
        // (The SILENT version of the same collision — `{4}`, a valid integer expression
        // that quietly becomes the digit 4 — is caught by the checker instead.)
        return Err(HelixError::new(
            format!("unexpected {} in interpolation `{{...}}`", p.peek().describe()),
            l,
            c,
        )
        .hint(
            "if this is a regex quantifier or a literal brace, use a RAW string (`\"\"\"[a-z]{2,}\"\"\"` interpolates nothing), or double the brace as `{{`.",
        ));
    }
    Ok((e, p.do_bindings))
}

/// `recv.sort_by(key)` - sort `recv` ascending by `key(element)`. Desugars to
/// `let $s = recv in $s.map(key).argsort().map($si => $s[$si])`, reusing the tested
/// map/argsort/index path so both engines handle it identically (parity by
/// construction). `$`-prefixed temporaries are unlexable, so they can't collide.
/// The help for "expected end of line after statement" — named for what the user was
/// *attempting*, not for what the parser was expecting.
///
/// It used to be one canned line: ``each statement goes on its own line; Helix has no `;` ``.
/// An adversarial sweep of 1438 programs written the way a newcomer would type them found
/// **109 of them getting that hint on a source containing no semicolon at all** — the single
/// largest diagnostic defect in the language. `for x in xs:` was told about semicolons.
///
/// A WRONG HINT IS WORSE THAN NO HINT: it sends the reader to look for a problem that is not
/// there, and it costs them the one thing the compiler actually knew. So the semicolon line
/// is now reserved for a source that HAS a semicolon, and everything else is answered by what
/// it opened with.
///
/// `for`, `while`, `def`, `function`, `lambda`, `return`, `elif`, `switch`, `case`, `end`,
/// `var` and `const` are NOT keywords in Helix — they lex as ordinary identifiers, which is
/// exactly why the parse dies one token later with no idea what was meant. Reserving them
/// would give a better message at the cost of breaking any program that uses one as a
/// variable name; matching on them here costs nothing and breaks nothing.
fn statement_boundary_hint(opened_with: &Tok, before: &Tok, found: &Tok) -> &'static str {
    // The import words are checked against what the statement OPENED with, and only when
    // a NAME follows — `use lib.util`, `from lib import util`. Unlike `for` or `lambda`,
    // `use` and `from` are plausible variable names (`from`/`to` is a natural pair in
    // scientific code), so matching them mid-statement or before an `=` would hand a
    // reader an import lecture about their own perfectly good binding. That is the exact
    // failure this function exists to prevent.
    //
    // Measured, not guessed: `use lib.util` and `from lib import util` both got
    // ``each statement goes on its own line; Helix has no `;` `` — a hint about a
    // semicolon that is not in the source, on the one mistake in a 14-case sweep that
    // Helix could answer BEST, because the feature exists and has three spellings.
    if let Tok::Ident(name) = opened_with
        && matches!(found, Tok::Ident(_))
        && matches!(name.as_str(), "use" | "from" | "using" | "require" | "include")
    {
        return "Helix imports by module path: `import stats`, `import lib.stats` for \
                `lib/stats.helix`, `import lib.stats as st` to alias it, or \
                `import lib.stats.{mean, sd}` to bring names in unqualified.";
    }
    // The foreign word can be what the statement OPENED with (`for x in xs:`) or the last
    // thing that parsed BEFORE the failure — `f = lambda x: …` opens with `f`, and
    // `fn f(x) = return x` opens with `fn`, so in both the word sits in the middle.
    for tok in [opened_with, before] {
        if let Tok::Ident(name) = tok {
            match name.as_str() {
            "for" => {
                return "Helix has no `for` loop — iterate with `xs.map(f)`, `xs.filter(p)`, \
                        `xs.reduce(init, f)`, or a tail-recursive function (which the JIT \
                        compiles to a real loop)."
            }
            "while" => {
                return "Helix has no `while` — a tail-recursive function IS the loop, and it \
                        runs in constant space: `fn go(n, acc) = if n == 0 then acc else go(n - 1, acc + n)`."
            }
            "def" | "function" | "func" => {
                return "define a function with `fn name(a, b) = expression` — one expression, \
                        no braces, no `return`."
            }
            "lambda" => return "a lambda is `x => x + 1`; pass it directly, e.g. `xs.map(x => x + 1)`.",
            "return" => {
                return "a function body is a single expression, so there is nothing to return \
                        from: `fn f(x) = x * 2`."
            }
            "elif" => return "write `else if`, and remember `if a then b else c` is an EXPRESSION.",
            "switch" | "case" => {
                return "use `match x { 0 => \"zero\", _ => \"other\" }` — every arm yields a value."
            }
            "end" | "endif" | "endfor" => {
                return "blocks are delimited by expression structure, not by `end` — there is \
                        nothing to close."
            }
            "var" | "const" | "let" => {
                return "a binding is just `name = value`; add `mut` to make one reassignable."
            }
            // `f"…"` — an f-string. The `f` parses as a name, then the string is the surprise.
            "f" | "rf" | "fr" if matches!(found, Tok::InterpStr(_) | Tok::Str(_)) => {
                return "Helix strings already interpolate — drop the `f`: `\"v={x}\"`."
            }
                _ => {}
            }
        }
    }
    // `(a, b) = (1, 2)` — destructuring a BINDING. The tuple parses as an expression and
    // the `=` is the surprise, so without this the reader is told about statement
    // boundaries: a message about a problem that is not there.
    //
    // What makes this worth its own arm is that the feature half-exists, so "not
    // supported" would be as misleading as the boundary hint. Destructuring a LAMBDA
    // parameter works today — `[(1, 2)].map((a, b) => a + b)` — and a field report listed
    // destructuring as flatly open without noticing. Naming the half that works turns a
    // dead end into a workaround.
    if matches!(opened_with, Tok::LParen) && matches!(found, Tok::Eq) {
        return "a binding takes one name — destructure by indexing (`p.0` / `p.1`), or in a \
                lambda parameter, where it DOES work: `xs.map((a, b) => a + b)`.";
    }
    // `x := 1` — Go's short declaration. The `x` parses as an expression and the `:` is the
    // surprise, so the statement neither opens nor ends with anything foreign-looking.
    if matches!(found, Tok::Colon) {
        return "a binding is just `name = value`; Helix has no `:=`, and `:` is only for \
                record fields and format specs.";
    }
    // `(int) 3.5`, `v = (float) x` — a C-style cast: a `)` immediately before a value.
    if matches!(before, Tok::RParen)
        && matches!(found, Tok::Int(_) | Tok::Float(_) | Tok::Ident(_) | Tok::Str(_))
    {
        return "Helix has no C-style casts — convert with `to_int(x)`, `to_float(x)` or `to_str(x)`.";
    }
    if matches!(found, Tok::InterpStr(_)) {
        return "Helix strings already interpolate — write `\"v={x}\"` with no prefix.";
    }
    "each statement goes on its own line; Helix has no `;`."
}

fn desugar_sort_by(recv: Expr, args: Vec<Expr>, l: usize, c: usize) -> Result<Expr, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new(
            format!("`sort_by` takes one key function, got {}", args.len()),
            l,
            c,
        )
        .hint("e.g. `people.sort_by(p => p.age)`."));
    }
    let key = args.into_iter().next().unwrap();
    let s = || Expr::Ident { name: "$s".to_string(), line: l, col: c };
    let keys = Expr::Method { recv: Box::new(s()), name: "map".into(), args: vec![key], named: vec![], ufcs: None, line: l, col: c };
    let order = Expr::Method { recv: Box::new(keys), name: "argsort".into(), args: vec![], named: vec![], ufcs: None, line: l, col: c };
    let gather = Expr::Lambda {
        params: vec!["$si".to_string()],
        body: Box::new(Expr::Index {
            recv: Box::new(s()),
            index: Box::new(Expr::Ident { name: "$si".to_string(), line: l, col: c }),
            line: l,
            col: c,
        }),
    };
    let body = Expr::Method { recv: Box::new(order), name: "map".into(), args: vec![gather], named: vec![], ufcs: None, line: l, col: c };
    Ok(Expr::Let { bindings: vec![("$s".to_string(), recv)], body: Box::new(body), from_do: false })
}

/// `recv.take_while(p)` / `recv.drop_while(p)` → take/drop the leading run where `p`
/// holds, via the first index where `p` is false (`?? count` so an all-true predicate
/// keeps/drops everything). `recv` is bound once (`$w`) to avoid double evaluation.
/// Desugared, so both engines get it for free.
///
/// The index comes from `position(p, false)` — the two-argument form, which the arity
/// check in [`desugar_position`] makes unwritable from source, so it is reachable only
/// from here. It used to be `map(p).index_of(false)`, which evaluated `p` over the WHOLE
/// receiver and materialized one `Value` per element to find an index that is usually
/// near the front: `(0..90_000_000).take_while(it < 5)` took **17.4s and 2.1 GB** to
/// answer `5`, while the already-lazy `any` answered the same shape in 0.07s and 14 MB.
/// `position` short-circuits, so both are now O(prefix).
/// `flat_map`/`count_where` — one-line compositions of existing verbs,
/// desugared so both engines get them for free (parity by construction):
/// `xs.flat_map(f)` is `xs.map(f).flatten()`; `xs.count_where(p)` is
/// `xs.filter(p).count()`. NOT here: `find` — Dna owns that name for motif
/// search, and a desugar is receiver-blind (this file's namespace-gate lesson),
/// so an Array `find` would hijack `seq.find("ATG")` at parse time. The
/// spelling for arrays stays `xs.filter(p).first()`.
fn desugar_filter_compose(
    recv: Expr,
    name: &str,
    mut args: Vec<Expr>,
    l: usize,
    c: usize,
) -> Result<Expr, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new(
            format!("`{}` takes one function, got {} arguments", name, args.len()),
            l,
            c,
        )
        .hint(match name {
            "flat_map" => "e.g. `xs.flat_map(x => [x, x])`.",
            _ => "e.g. `xs.count_where(x => x > 0)`.",
        }));
    }
    // Cannot panic: the arity check above guarantees exactly one element.
    let f = args.pop().unwrap();
    let m = |recv: Expr, nm: &str, args: Vec<Expr>| Expr::Method {
        recv: Box::new(recv),
        name: nm.into(),
        args,
        named: vec![],
        ufcs: None,
        line: l,
        col: c,
    };
    Ok(match name {
        "flat_map" => m(m(recv, "map", vec![f]), "flatten", vec![]),
        _ => m(m(recv, "filter", vec![f]), "count", vec![]),
    })
}

fn desugar_take_drop_while(recv: Expr, name: &str, mut args: Vec<Expr>, l: usize, c: usize) -> Result<Expr, HelixError> {
    if args.len() != 1 {
        return Err(HelixError::new(
            format!("`{}` takes one predicate function, got {}", name, args.len()),
            l,
            c,
        )
        .hint("e.g. `xs.take_while(x => x > 0)`."));
    }
    let p = args.pop().unwrap();
    let w = || Expr::Ident { name: "$w".to_string(), line: l, col: c };
    let m = |recv: Expr, nm: &str, args: Vec<Expr>| Expr::Method {
        recv: Box::new(recv),
        name: nm.into(),
        args,
        named: vec![],
        ufcs: None,
        line: l,
        col: c,
    };
    let idx = m(w(), "position", vec![p, Expr::Bool(false)]);
    let stop = Expr::Binary {
        op: BinOp::Coalesce,
        left: Box::new(idx),
        right: Box::new(m(w(), "count", vec![])),
        line: l,
        col: c,
    };
    let verb = if name == "take_while" { "take" } else { "drop" };
    let body = m(w(), verb, vec![stop]);
    Ok(Expr::Let { bindings: vec![("$w".to_string(), recv)], body: Box::new(body), from_do: false })
}

/// Let a higher-order method take a *named function* as its single argument:
/// `xs.map(normalize)` / `xs.any(is_valid)` → `xs.map(it => normalize(it))`. Without this, a
/// bare identifier is the implicit-`it` body, so `map(g)` maps every element to the *value*
/// `g` (the function itself) rather than applying it. Only a bare `Ident` other than the
/// binder `it` is wrapped — `map(it)` (identity) and `map(it * 2)` (a real body) are
/// untouched; mapping to a constant via a bare name was meaningless anyway. Works for a
/// top-level `fn` or a function-valued variable (the call resolves either).
///
/// A DOTTED path is wrapped the same way: `xs.map(util.double)` → `xs.map(it => util.double(it))`.
/// An imported module's function is the ordinary way to pass a library function to `map`, and
/// without this it silently produced `[<function/1>, <function/1>, …]` — an array of the
/// function value, exit 0, no diagnostic. The rewrite is `Field{recv, name}` →
/// `Method{recv, name, args:[it]}`, which is exactly how `util.double(it)` parses anyway.
///
/// A path ROOTED AT `it` is never wrapped, because that is a real body: `xs.map(it.name)`
/// projects a field out of each element and must stay a projection.
///
/// Restricted to the array-EXCLUSIVE higher-order methods (`map`/`any`/`all`). `filter`/
/// `where` are deliberately NOT wrapped: they are also DataFrame column-verbs, where
/// `df.where(strong)` is a bare *column* reference, not a function — and parse time can't
/// tell the receiver apart. For an array `filter`/`where` with a named predicate, use an
/// explicit lambda (`xs.filter(x => is_valid(x))`).
fn wrap_bound_fn_arg(name: &str, args: Vec<Expr>, l: usize, c: usize) -> Vec<Expr> {
    if !matches!(name, "map" | "any" | "all") || args.len() != 1 {
        return args;
    }
    let it = || Expr::Ident { name: "it".to_string(), line: l, col: c };
    let body = match &args[0] {
        Expr::Ident { name: f, .. } if f != "it" => {
            Expr::Call { name: f.clone(), args: vec![it()], line: l, col: c }
        }
        Expr::Field { recv, name: f, .. } if !path_rooted_at_it(recv) => Expr::Method {
            recv: recv.clone(),
            name: f.clone(),
            args: vec![it()],
            named: vec![],
            ufcs: None,
            line: l,
            col: c,
        },
        _ => return args,
    };
    vec![Expr::Lambda { params: vec!["it".to_string()], body: Box::new(body) }]
}

/// Is this dotted path rooted at the implicit binder `it` (`it.a.b`)? Such a path is a
/// projection out of each element, not a function to apply — see [`wrap_bound_fn_arg`].
fn path_rooted_at_it(e: &Expr) -> bool {
    match e {
        Expr::Ident { name, .. } => name == "it",
        Expr::Field { recv, .. } => path_rooted_at_it(recv),
        _ => false,
    }
}

/// `recv.position(p)` → the first index where `p` holds, or `missing`. Just
/// `recv.map(p).index_of(true)` (no double-eval — `recv` is used once).
fn desugar_position(recv: Expr, args: Vec<Expr>, l: usize, c: usize) -> Result<Expr, HelixError> {
    // ONE argument from source. The two-argument form — `position(p, want)`, the first
    // index whose predicate result is exactly `Bool(want)` — is generated by
    // `desugar_take_drop_while` and is unwritable here, which is what keeps the extra
    // parameter out of the language's surface (ADR 0003: one verb per concept).
    if args.len() != 1 {
        return Err(HelixError::new(
            format!("`position` takes one predicate function, got {}", args.len()),
            l,
            c,
        )
        .hint("e.g. `xs.position(x => x == target)`."));
    }
    // No longer `map(p).index_of(true)`: that ran the predicate over every element and
    // built a full array of results to find an index that short-circuiting reaches
    // immediately. Both engines now compile `position` as a short-circuiting scan, so it
    // is O(prefix) in time and O(1) in space. See `desugar_take_drop_while`.
    Ok(Expr::Method { recv: Box::new(recv), name: "position".into(), args, named: vec![], ufcs: None, line: l, col: c })
}

/// Desugar `recv.min_by(key)` / `max_by(key)` / `argmin()` / `argmax()` into
/// existing constructs so both engines handle them with no new ops:
///
///   recv.min_by(p => K)  →  let $o = recv.map(p => (K, p))
///                           in $o.reduce($o[0], ($a, $b) => if $b[0] < $a[0] then $b else $a)[1]
///   recv.argmin()        →  let $o = recv.enumerate()
///                           in $o.reduce($o[0], ($a, $b) => if $b[1] < $a[1] then $b else $a)[0]
///
/// `min_by`/`max_by` reduce over `(key, element)` pairs and return the element;
/// `argmin`/`argmax` reduce over `(index, value)` pairs and return the index.
fn desugar_order_by(
    recv: Expr,
    name: &str,
    mut args: Vec<Expr>,
    line: usize,
    col: usize,
) -> Result<Expr, HelixError> {
    use crate::ast::BinOp;
    let by = name == "min_by" || name == "max_by";
    // `argmax`/`max_by` take the greater element, `argmin`/`min_by` the lesser. Captured as a
    // bool as well as a `BinOp` because `BinOp` is neither `Copy` nor `PartialEq`, and the
    // packed kernel below needs the same decision after `op` has been moved into the lambda.
    let want_max = !(name == "min_by" || name == "argmin");
    let op = if want_max { BinOp::Gt } else { BinOp::Lt };

    let ident = |n: &str| Expr::Ident { name: n.to_string(), line, col };
    let index = |e: Expr, i: i64| Expr::Index {
        recv: Box::new(e),
        index: Box::new(Expr::Int(i)),
        line,
        col,
    };

    // The source array of comparison pairs, and which slot is the key vs. the result.
    let (src, key_idx, ret_idx) = if by {
        // `min_by`/`max_by(key)` — extract the key body (`p => K`, a destructuring
        // `(k, n) => K`, or an implicit-`it` bare expression).
        let (params, key) = match args.len() {
            1 => match args.pop().unwrap() {
                Expr::Lambda { params, body } if !params.is_empty() => (params, *body),
                Expr::Lambda { .. } => {
                    return Err(HelixError::new(
                        format!("`{name}` needs a key function with at least one parameter"),
                        line,
                        col,
                    )
                    .hint("e.g. `rows.min_by(r => r.score)` or `pairs.max_by((k, n) => n)`."))
                }
                other => (vec!["it".to_string()], other),
            },
            _ => {
                return Err(HelixError::new(
                    format!("`{name}` takes exactly one key function"),
                    line,
                    col,
                )
                .hint("e.g. `rows.min_by(r => r.score)`."))
            }
        };
        // `min_by`/`max_by` return the element AT the winning key's index:
        //
        //     recv.min_by(p => K)  →  let $obe = recv in $obe[$obe.map(p => K).argmin()]
        //
        // (with the argmin itself desugared by the recursive call below). An earlier
        // desugar instead mapped to `(K, elem)` pairs and, for a DESTRUCTURING key like
        // `(a, b) => K`, rebuilt `elem` as `Expr::Tuple(binders)` — with a comment
        // claiming that "rebuilds the original element". It does not: Helix destructures
        // Arrays too, so `[[1,2],[0,3]].min_by((a, b) => a)` returned the Tuple `(0, 3)`
        // where the single-binder spelling of the same query returned the Array `[0, 3]`
        // — two spellings, two TYPES. Indexing the bound receiver returns the original
        // element whatever it is.
        //
        // The key map keeps the USER'S lambda, so every destructure diagnostic ("cannot
        // destructure a value of type Int into 2 parameters", "lambda expects 2 values,
        // but the element has 3") is produced by the same machinery at the same moment.
        // And the error matrix beyond that is preserved because min_by's errors were
        // always argmin's errors wearing a different name: `[].min_by(it)` and
        // `[].argmin()` both leak the same reduce seed ("index 0 is out of bounds"),
        // `missing.min_by(it)` and `missing.argmin()` the same "cannot be indexed", NaN
        // keys the same "cannot compare", missing keys the same "`if` condition is
        // `missing`", and both break ties FIRST-wins — verified byte-for-byte on all
        // three engines before this was written.
        let keys = Expr::Method {
            recv: Box::new(ident("$obe")),
            name: "map".to_string(),
            args: vec![Expr::Lambda { params, body: Box::new(key) }],
            named: vec![],
            ufcs: None,
            line,
            col,
        };
        let inner = if name == "min_by" { "argmin" } else { "argmax" };
        let idx_expr = desugar_order_by(keys, inner, Vec::new(), line, col)?;
        // ADR 0025 (c1): the REDUCTION policy, spelled in ordinary AST. The receiver's own
        // guards carry THIS spelling's name (an empty `min_by` must not say "argmin"), and
        // the missing/NaN policy rides on the inner argmin, whose `missing` answer is
        // caught here — `$obe[missing]` would raise "`index` expected an integer", which is
        // exactly the leaked-internals shape c1 removes. `is_missing`/`count` are O(1), so
        // the composed fast path (`a5737ce`) is undisturbed.
        let method0 = |recv: Expr, m: &str| Expr::Method {
            recv: Box::new(recv),
            name: m.to_string(),
            args: vec![],
            named: vec![],
            ufcs: None,
            line,
            col,
        };
        let indexed = Expr::Let {
            from_do: false,
            bindings: vec![("$obi".to_string(), idx_expr)],
            body: Box::new(Expr::If {
                cond: Box::new(method0(ident("$obi"), "is_missing")),
                then_branch: Box::new(Expr::Missing),
                else_branch: Box::new(Expr::Index {
                    recv: Box::new(ident("$obe")),
                    index: Box::new(ident("$obi")),
                    line,
                    col,
                }),
                line,
                col,
            }),
        };
        return Ok(Expr::Let {
            from_do: false,
            bindings: vec![("$obe".to_string(), recv)],
            body: Box::new(Expr::If {
                cond: Box::new(method0(ident("$obe"), "is_missing")),
                then_branch: Box::new(Expr::Missing),
                else_branch: Box::new(Expr::If {
                    cond: Box::new(Expr::Binary {
                        op: BinOp::Eq,
                        left: Box::new(method0(ident("$obe"), "count")),
                        right: Box::new(Expr::Int(0)),
                        line,
                        col,
                    }),
                    // `raise` is the ordinary builtin (task #8); a user `fn raise` would
                    // capture it under ADR 0027's file scoping, the same way any desugar
                    // that names a builtin can be re-pointed. Accepted: the capture is the
                    // user's own file-scoped choice, and the alternative is a private AST
                    // node for one message.
                    then_branch: Box::new(Expr::Call {
                        name: "raise".to_string(),
                        args: vec![Expr::Str(format!("`{name}` of an empty collection"))],
                        line,
                        col,
                    }),
                    else_branch: Box::new(indexed),
                    line,
                    col,
                }),
                line,
                col,
            }),
        });
    } else {
        // `argmin`/`argmax()` — pairs are `(index, value)` from `enumerate`.
        if !args.is_empty() {
            return Err(HelixError::new(format!("`{name}` takes no arguments"), line, col)
                .hint("use `min_by`/`max_by` to order by a key."));
        }
        // The receiver is bound to `$oba` below and referenced twice — once by the packed
        // kernel, once by this enumerate — so it is evaluated exactly once either way.
        let enumd = Expr::Method {
            recv: Box::new(ident("$oba")),
            name: "enumerate".to_string(),
            args: vec![],
            named: vec![],
            ufcs: None,
            line,
            col,
        };
        (enumd, 1, 0)
    };

    // ($a, $b) => if $b[key] OP $a[key] then $b else $a
    let cmp = Expr::Lambda {
        params: vec!["$ob_a".to_string(), "$ob_b".to_string()],
        body: Box::new(Expr::If {
            cond: Box::new(Expr::Binary {
                op,
                left: Box::new(index(ident("$ob_b"), key_idx)),
                right: Box::new(index(ident("$ob_a"), key_idx)),
                line,
                col,
            }),
            then_branch: Box::new(ident("$ob_b")),
            else_branch: Box::new(ident("$ob_a")),
            line,
            col,
        }),
    };
    let reduced = Expr::Method {
        recv: Box::new(ident("$ob")),
        name: "reduce".to_string(),
        args: vec![index(ident("$ob"), 0), cmp],
        named: vec![],
        ufcs: None,
        line,
        col,
    };
    // The tuple reduce above is the ONLY implementation `argmin`/`argmax` had, and it cost
    // 11x the manual `index_of(max())` spelling at n=1e6 and 24x at n=1e7 — the gap widening
    // with n because the price is one heap-allocated `Tuple` per element. No engine can help:
    // the JIT's reduce lowering wants a pure i64 scalar body and this accumulator is a tuple,
    // so all three engines walk the same allocations.
    //
    //     xs.argmax()  →  let $oba = xs in $oba.$arg_extreme(true) ?? <the reduce above>
    //
    // `$arg_extreme` is unwritable from source (`$` does not lex), so it is an internal verb
    // in the same family as the `$ob`/`$obe` binders already here and `desugar_position`'s
    // hidden flag argument. It answers `missing` to DECLINE, and the `??` then runs the
    // byte-identical AST that ran before — so every declined shape keeps its exact error
    // text, caret column and help line, produced by the same machinery at the same moment.
    // That is what preserves the leaks this desugar is known for, including
    // `missing.argmax()` → "a value of type Missing cannot be indexed" and `[].argmax()` →
    // "index 0 is out of bounds for length 0"; both are the reduce seed talking, and only
    // the real reduce says them with the right column.
    //
    // `missing` is safe as the decline sentinel for a precise reason: the KERNEL never
    // produces `missing` as an ANSWER — a packed array cannot contain `missing`, and the
    // kernel declines on NaN. The method as a whole DOES answer `missing` now (ADR 0025
    // (c1): missing/NaN propagate), but every such answer is produced by the policy guards
    // on the RIGHT of the `??` — the slow path, where it is final and nothing downstream
    // needs to tell it from a decline. That guard placement is what resolved the collision
    // this comment used to warn about, without a second channel; keep the guards there,
    // or the warning becomes live again.
    //
    // `min_by`/`max_by` inherit the fast path for free: they compose through the recursive
    // `desugar_order_by(keys, inner, …)` call above, and the key `map` produces a packed
    // column, so the kernel takes it.
    let slow = Expr::Let {
        from_do: false,
        bindings: vec![("$ob".to_string(), src)],
        body: Box::new(index(reduced, ret_idx)),
    };
    // ADR 0025 (c1): the method adopts the REDUCTION policy — `missing`/NaN propagate as
    // `missing`, the empty array gets the free function's own named error — spelled as
    // ordinary AST guards ON THE SLOW PATH ONLY. Placement is both the soundness and the
    // performance argument:
    //
    //   * SOUND without a new decline channel: `$arg_extreme` still answers `missing` to
    //     mean "declined", and nothing downstream has to distinguish that from a real
    //     `missing` answer — the kernel only ever answers an Int (a packed array cannot
    //     contain `missing`, and it declines on NaN), so a real `missing` is only ever
    //     produced HERE, on the right of the `??`, where it is final. The collision the
    //     comment above warns about never materializes.
    //   * FAST: packed Ints/Floats/Range never reach the guards, so the kernel path is
    //     byte-for-byte what it was. The guards' extra scans run only on shapes that were
    //     already walking the interpreted tuple-reduce.
    //
    // Guard order is load-bearing: `is_missing` first (`missing.count()` would propagate
    // and make the `if` condition `missing` — the exact leak this removes), then empty,
    // then missing-elements, then NaN. The missing check is `count() !=
    // drop_missing().count()`, NOT `any(it.is_missing())` — `any` propagates a missing
    // ELEMENT before the predicate runs, so that spelling answers `missing` for the wrong
    // reason on some shapes and was measured doing so. The NaN check is `any(it != it)`,
    // total once missing elements are excluded (IEEE: only NaN is unequal to itself).
    let method0 = |recv: Expr, m: &str| Expr::Method {
        recv: Box::new(recv),
        name: m.to_string(),
        args: vec![],
        named: vec![],
        ufcs: None,
        line,
        col,
    };
    let count_of = |e: Expr| -> Expr {
        Expr::Method {
            recv: Box::new(e),
            name: "count".to_string(),
            args: vec![],
            named: vec![],
            ufcs: None,
            line,
            col,
        }
    };
    let guarded_slow = Expr::If {
        cond: Box::new(method0(ident("$oba"), "is_missing")),
        then_branch: Box::new(Expr::Missing),
        else_branch: Box::new(Expr::If {
            cond: Box::new(Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(count_of(ident("$oba"))),
                right: Box::new(Expr::Int(0)),
                line,
                col,
            }),
            then_branch: Box::new(Expr::Call {
                name: "raise".to_string(),
                args: vec![Expr::Str(format!("`{name}` of an empty collection"))],
                line,
                col,
            }),
            else_branch: Box::new(Expr::If {
                cond: Box::new(Expr::Binary {
                    op: BinOp::Ne,
                    left: Box::new(count_of(ident("$oba"))),
                    right: Box::new(count_of(method0(ident("$oba"), "drop_missing"))),
                    line,
                    col,
                }),
                then_branch: Box::new(Expr::Missing),
                // A NaN answers with its own INDEX, not `missing` (ADR 0036 policy 3).
                //
                // `min()` on a NaN-bearing array returns the NaN, and the NaN sits at
                // this index — so `xs[xs.argmin()] == xs.min()` holds, which is the
                // invariant worth keeping and is exactly what numpy does
                // (`np.argmin([1.0, nan, 3.0])` is 1). Until v0.6.0 this branch
                // answered `missing`, which was the laundering written into the
                // desugar itself: a computation that FAILED reported as absent data.
                //
                // `it != it` is true only for a NaN, and `position` is guaranteed to
                // find one because that is the condition guarding this branch.
                else_branch: Box::new(Expr::If {
                    cond: Box::new(Expr::Method {
                        recv: Box::new(ident("$oba")),
                        name: "any".to_string(),
                        args: vec![Expr::Binary {
                            op: BinOp::Ne,
                            left: Box::new(ident("it")),
                            right: Box::new(ident("it")),
                            line,
                            col,
                        }],
                        named: vec![],
                        ufcs: None,
                        line,
                        col,
                    }),
                    then_branch: Box::new(Expr::Method {
                        recv: Box::new(ident("$oba")),
                        name: "position".to_string(),
                        args: vec![Expr::Lambda {
                            params: vec!["$nanq".to_string()],
                            // `x != x` rather than `is_nan(x)`: this desugar is
                            // generated for EVERY receiver type, and `is_nan` on a
                            // String is a static type error — so spelling it that way
                            // broke `["a", "b"].min_by(it)` at check time even though
                            // the branch can never run for strings. `!=` is defined on
                            // every type and is true only for a NaN, which is exactly
                            // the test the guard one level up already uses.
                            body: Box::new(Expr::Binary {
                                op: BinOp::Ne,
                                left: Box::new(ident("$nanq")),
                                right: Box::new(ident("$nanq")),
                                line,
                                col,
                            }),
                        }],
                        named: vec![],
                        ufcs: None,
                        line,
                        col,
                    }),
                    else_branch: Box::new(slow),
                    line,
                    col,
                }),
                line,
                col,
            }),
            line,
            col,
        }),
        line,
        col,
    };
    Ok(Expr::Let {
        from_do: false,
        bindings: vec![("$oba".to_string(), recv)],
        body: Box::new(Expr::Binary {
            op: BinOp::Coalesce,
            left: Box::new(Expr::Method {
                recv: Box::new(ident("$oba")),
                name: "$arg_extreme".to_string(),
                args: vec![Expr::Bool(want_max)],
                named: vec![],
                ufcs: None,
                line,
                col,
            }),
            right: Box::new(guarded_slow),
            line,
            col,
        }),
    })
}

const TYPE_NAMES: &[&str] = &[
    "Int", "Float", "Num", "String", "Bool", "Array", "Tensor", "DataFrame", "Dna",
];

/// One user-written `do {}` binding: its name and where it was written.
type DoBinding = (String, usize, usize);

/// `mut` written inside a body, where it is not a statement. The bare "unexpected `mut`"
/// this used to produce read as "mutability is unsupported here, give up"; in fact the
/// thing the author wants already works, spelled without the keyword — a `do {}` block
/// rebinds a name by shadowing, line after line. Naming that is the whole fix.
fn mut_inside_a_body(line: usize, col: usize) -> HelixError {
    HelixError::new("`mut` declares a top-level binding, so it cannot appear here", line, col)
        .hint(
            "a body rebinds by name, with no keyword — `do { n = 0` / `n = n + 1` / `n }` \
             evaluates to 1, each line shadowing the last. For state that must OUTLIVE the \
             call, declare `mut` at the top level; to carry state across a sequence, thread \
             it with `reduce`.",
        )
}

pub fn parse(tokens: Vec<Token>) -> Result<Vec<Stmt>, HelixError> {
    // Every `fn NAME` in the file, before a line of it is parsed. A function may be
    // called above its definition, so the UFCS fallback cannot wait for the definition
    // to be reached; the scan is lexical and needs no structure.
    let fn_names: std::collections::HashSet<String> = tokens
        .windows(2)
        .filter_map(|w| match (&w[0].tok, &w[1].tok) {
            (Tok::Fn, Tok::Ident(n)) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        depth: 0,
        fn_sigs: HashMap::new(),
        do_bindings: Vec::new(),
        imports: std::collections::HashSet::new(),
        selected_imports: std::collections::HashSet::new(),
        fn_names,
    };
    let program = p.program()?;
    reject_do_binding_over_mut_global(&program, &p.do_bindings)?;
    Ok(program)
}

/// A `do {}` binding may not reuse the name of a `mut` global.
///
/// `do {}` bindings are immutable — the block desugars to `let … in` — so
///
///     mut n = 0
///     fn bump() = do { n = n + 1
///                      n }
///
/// binds a NEW local `n` from the global's value and throws it away at the end of the
/// block: `bump()` twice printed `1`, `1`, and the global stayed `0`. Exit 0, no
/// diagnostic. Nobody writes that intending a shadow, and the `mut` is what proves it —
/// the author declared the name mutable and then wrote what looks like an assignment.
///
/// Only `mut` globals are rejected. Shadowing an *immutable* global stays legal, and so
/// does an explicit `let n = … in …`: both are unambiguous about binding rather than
/// updating. The parser can decide this alone because it sees the whole file — the
/// top-level statements and every `do {}` binding in it, whatever their order.
fn reject_do_binding_over_mut_global(
    program: &[Stmt],
    do_bindings: &[DoBinding],
) -> Result<(), HelixError> {
    let mut muts: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for s in program {
        match s {
            Stmt::Assign { name, mutable: true, .. } => {
                muts.insert(name.as_str());
            }
            Stmt::Destructure { names, mutable: true, .. } => {
                muts.extend(names.iter().map(String::as_str));
            }
            _ => {}
        }
    }
    for (name, line, col) in do_bindings {
        if muts.contains(name.as_str()) {
            return Err(HelixError::new(
                format!("`{name}` is a mutable global; this binding would shadow it, not update it"),
                *line,
                *col,
            )
            .hint(format!(
                "`do {{}}` bindings are immutable, so `{name} = …` here creates a NEW local \
                 `{name}` and the global keeps its old value. Return the new value and assign \
                 it where the global lives (`{name} = step({name})`), or rename the local."
            )));
        }
    }
    Ok(())
}

/// Caps how deeply expressions may nest. Every recursive descent path (groups,
/// prefixes, calls, arrays, indexing) funnels through `unary`, so a counter
/// there bounds parser recursion — and, since it bounds AST nesting depth, it
/// also protects the type checker / compiler / tree-walker, which recurse over
/// the same shape. Deep enough nested input would otherwise overflow the stack
/// at parse time rather than producing a clean error.
const MAX_PARSE_DEPTH: usize = 1000;

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: usize,
    /// Signatures of user functions seen so far, keyed by name — for resolving
    /// named arguments and defaults at call sites (a function is defined before it
    /// is called, so its signature is known by the time its calls are parsed).
    fn_sigs: HashMap<String, FnSig>,
    /// Every user-written `do {}` binding, collected as the file is parsed and checked
    /// against the top-level `mut` globals once the whole program is known. See
    /// [`reject_do_binding_over_mut_global`]. Generated `$do<N>` bindings are not
    /// recorded: they are throwaway names the user never wrote.
    do_bindings: Vec<DoBinding>,
    /// Namespaces bound by `import` so far — an alias, or the path's last segment.
    /// The comprehension-sugar desugars consult this: a method on an imported
    /// namespace is a QUALIFIED MODULE CALL, never array sugar, however it is named.
    imports: std::collections::HashSet<String>,
    /// Names bound by SELECTIVE imports (`import m.{f}`) — for diagnostics only:
    /// a named-argument call on one is unresolvable in this file (the signature
    /// lives in the imported module), and the error should say so, not call `f`
    /// a builtin.
    selected_imports: std::collections::HashSet<String>,
    /// Every name introduced by a `fn` anywhere in this file, pre-scanned from the
    /// token stream before parsing begins.
    ///
    /// A function may be called above its definition, so the UFCS fallback below
    /// cannot wait until the definition is parsed to know the name exists. The scan is
    /// purely lexical — `fn` followed by an identifier — and deliberately
    /// over-approximates by including nested functions: the cost of doing so is that
    /// `x.f()` for an out-of-scope `f` reports "`f` is not defined" instead of "no
    /// method `f`", and the cost of under-approximating would be a call that works in
    /// one file and not in another.
    fn_names: std::collections::HashSet<String>,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_tok(&self) -> &Token {
        &self.toks[self.pos]
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let idx = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[idx].tok
    }

    /// The ADR 0035 `where` clause: after a fn body, `where NAME = EXPR`
    /// (comma-separated, later bindings seeing earlier ones) wraps the body in
    /// the equivalent `let`. Answers the body unchanged when the next tokens
    /// are not exactly that shape.
    fn maybe_where_clause(&mut self, body: Expr) -> Result<Expr, HelixError> {
        let mut k = 0usize;
        while matches!(self.peek_at(k), Tok::Newline) {
            k += 1;
        }
        let gate = matches!(self.peek_at(k), Tok::Ident(n) if n == "where")
            && matches!(self.peek_at(k + 1), Tok::Ident(_))
            && matches!(self.peek_at(k + 2), Tok::Eq);
        if !gate {
            // NEAR-MISS teaching (the sweep's three natural mistakes all fell
            // through to a generic "no `;`" error that misdescribed them):
            // `where` followed by a NAME but no `=` is a malformed clause —
            // a missing `=`, an attempted destructure, or a stray token —
            // never two statements.
            if matches!(self.peek_at(k), Tok::Ident(n) if n == "where")
                && matches!(self.peek_at(k + 1), Tok::Ident(_))
            {
                for _ in 0..=k {
                    self.advance();
                }
                let (l, c) = self.pos();
                return Err(HelixError::new(
                    "malformed `where` clause after this function",
                    l,
                    c,
                )
                .hint(
                    "a binding is `where NAME = value`; several separate with commas \
                     (`where a = 1, b = a + 2`). One `where` per fn, and no destructuring.",
                ));
            }
            return Ok(body);
        }
        for _ in 0..=k {
            self.advance(); // the newline run and `where` itself
        }
        let mut bindings: Vec<(String, Expr)> = Vec::new();
        loop {
            let name = self.ident_name("after `where`")?;
            self.eat(&Tok::Eq, "after the `where` binding's name")
                .map_err(|e| e.hint("a `where` binding looks like `where LOOKUP = {…}`."))?;
            let value = self.expr()?;
            bindings.push((name, value));
            if matches!(self.peek(), Tok::Comma) {
                let (cl, cc) = self.pos();
                self.advance();
                while matches!(self.peek(), Tok::Newline) {
                    self.advance();
                }
                // A comma must be FOLLOWED by a binding — otherwise it would
                // swallow the next statement's first token as a binding name
                // and caret a perfectly well-formed line (the sweep's
                // trailing-comma finding).
                if !(matches!(self.peek(), Tok::Ident(_))
                    && matches!(self.peek_at(1), Tok::Eq))
                {
                    return Err(HelixError::new(
                        "expected another binding after the comma in this `where` clause",
                        cl,
                        cc,
                    )
                    .hint("remove the trailing comma, or add the binding: `where a = 1, b = 2`."));
                }
                continue;
            }
            break;
        }
        // A SECOND stacked `where` line is the natural Haskell-style spelling —
        // teach the comma form instead of letting it fall through to a generic
        // statement error.
        let mut k2 = 0usize;
        while matches!(self.peek_at(k2), Tok::Newline) {
            k2 += 1;
        }
        if matches!(self.peek_at(k2), Tok::Ident(n) if n == "where")
            && matches!(self.peek_at(k2 + 1), Tok::Ident(_))
        {
            for _ in 0..=k2 {
                self.advance();
            }
            let (l, c) = self.pos();
            return Err(HelixError::new(
                "a function takes ONE `where` clause",
                l,
                c,
            )
            .hint("separate the bindings with commas: `where a = 1, b = 2`."));
        }
        Ok(Expr::Let { bindings, body: Box::new(body), from_do: false })
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_end(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn pos(&self) -> (usize, usize) {
        let t = self.peek_tok();
        (t.line, t.col)
    }

    fn eat(&mut self, want: &Tok, ctx: &str) -> Result<Token, HelixError> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(want) {
            Ok(self.advance())
        } else {
            let (l, c) = self.pos();
            Err(HelixError::new(
                format!("expected {} {}, found {}", want.describe(), ctx, self.peek().describe()),
                l,
                c,
            ))
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.advance();
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>, HelixError> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.at_end() {
            // What the statement STARTED with, kept for the diagnostic below. `for`, `while`,
            // `def`, `lambda` and `return` are not keywords here — they lex as ordinary
            // identifiers — so by the time the parse fails, the only trace of what the user
            // was actually attempting is this token.
            let opened_with = self.peek().clone();
            let s = self.statement()?;
            stmts.push(s);
            if self.at_end() {
                break;
            }
            // statements are separated by at least one newline
            if matches!(self.peek(), Tok::Newline) {
                self.skip_newlines();
            } else {
                let (l, c) = self.pos();
                let found = self.peek().clone();
                // The token JUST BEFORE the failure matters as much as the one the statement
                // opened with: `f = lambda x: …` opens with `f`, and `fn f(x) = return x`
                // opens with `fn` — in both, the foreign word sits in the middle and is the
                // last thing that parsed.
                let before = self.toks[self.pos.saturating_sub(1)].tok.clone();
                return Err(HelixError::new(
                    format!("expected end of line after statement, found {}", found.describe()),
                    l,
                    c,
                )
                .hint(statement_boundary_hint(&opened_with, &before, &found)));
            }
        }
        Ok(stmts)
    }

    fn statement(&mut self) -> Result<Stmt, HelixError> {
        // `import a.b.c [as alias]` — load the module at `a/b/c.helix`.
        if matches!(self.peek(), Tok::Import) {
            let (l, c) = self.pos();
            self.advance();
            let first = self
                .ident_name("after `import`")
                .map_err(|e| e.hint("import a module by name, e.g. `import stats` or `import lib.stats`."))?;
            let mut segments = vec![first];
            let mut selected: Option<Vec<String>> = None;
            // Dotted path: `import lib.stats` → `lib/stats.helix`. A `.{a, b}` tail
            // instead selects names to bring into scope unqualified.
            while matches!(self.peek(), Tok::Dot) {
                self.advance();
                if matches!(self.peek(), Tok::LBrace) {
                    self.advance(); // consume `{`
                    let mut names = Vec::new();
                    loop {
                        names.push(self.ident_name("to import inside `{ }`")?);
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                    self.eat(&Tok::RBrace, "to close the import list").map_err(|e| {
                        e.hint("a selective import looks like `import lib.stats.{mean, std}`.")
                    })?;
                    selected = Some(names);
                    break; // nothing follows a selective list
                }
                segments.push(self.ident_name("after `.` in the module path")?);
            }
            // Optional `as alias` — `as` is contextual (only special here), so it
            // stays usable as an ordinary identifier everywhere else. A selective
            // import binds its names directly, so it takes no alias.
            let alias = if selected.is_none() && matches!(self.peek(), Tok::Ident(n) if n == "as") {
                self.advance();
                self.ident_name("after `as`")
                    .map_err(|e| e.hint("give the module an alias, e.g. `import lib.stats as stats`."))?
            } else {
                // Default the namespace to the last path segment.
                segments.last().unwrap().clone()
            };
            // Record the bound namespace so the comprehension-sugar desugars know a
            // method on it is a qualified module call. Selective imports bind function
            // names directly and create no namespace, so they are not recorded.
            if let Some(names) = &selected {
                self.selected_imports.extend(names.iter().cloned());
            } else {
                self.imports.insert(alias.clone());
            }
            return Ok(Stmt::Import { segments, alias, selected, line: l, col: c });
        }
        // Optional leading `export` (ADR 0019) — contextual: it's a keyword only
        // directly before a definition (`export fn …`, `export x = …`, `export a, b = …`,
        // `export mut …`), so `export` stays a usable identifier everywhere else.
        let exported = matches!(self.peek(), Tok::Ident(n) if n == "export")
            && match self.peek_at(1) {
                Tok::Fn | Tok::Mut => true,
                Tok::Ident(_) => matches!(self.peek_at(2), Tok::Eq | Tok::Comma),
                _ => false,
            };
        if exported {
            self.advance(); // consume `export`
        }

        // `fn name(a, b) = expr`
        if matches!(self.peek(), Tok::Fn) {
            let (l, c) = self.pos();
            self.advance();
            let name = self.ident_name("after `fn`")?;
            self.eat(&Tok::LParen, "to start the parameter list")
                .map_err(|e| e.hint("functions look like `fn area(w, h) = w * h`."))?;
            let mut params: Vec<(String, Option<TypeAnn>)> = Vec::new();
            let mut defaults: Vec<Option<Expr>> = Vec::new();
            if !matches!(self.peek(), Tok::RParen) {
                loop {
                    let (pl, pc) = self.pos();
                    let pname = self.ident_name("as a parameter")?;
                    // optional `: Type` annotation
                    let ann = if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        Some(self.parse_type_ann()?)
                    } else {
                        None
                    };
                    // optional `= literal` default value
                    let default = if matches!(self.peek(), Tok::Eq) {
                        self.advance();
                        let d = self.expr()?;
                        if !is_const_default(&d) {
                            return Err(HelixError::new(
                                format!("the default for parameter `{pname}` must be a literal constant"),
                                pl,
                                pc,
                            )
                            .hint("defaults like `= 0`, `= -5`, `= \"\"`, `= true`, or `= missing` are allowed."));
                        }
                        Some(d)
                    } else {
                        None
                    };
                    // Parameters with defaults must come last, so positional binding is
                    // unambiguous.
                    if default.is_none() && defaults.iter().any(|d| d.is_some()) {
                        return Err(HelixError::new(
                            format!("parameter `{pname}` has no default but follows one that does"),
                            pl,
                            pc,
                        )
                        .hint("put parameters with defaults after those without."));
                    }
                    params.push((pname, ann));
                    defaults.push(default);
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            self.eat(&Tok::RParen, "to close the parameter list")?;
            // Record the signature BEFORE parsing the body, so a recursive call inside
            // it resolves named arguments and defaults against this function.
            self.fn_sigs.insert(
                name.clone(),
                FnSig {
                    params: params.iter().map(|(n, _)| n.clone()).collect(),
                    defaults: defaults.clone(),
                },
            );
            // optional `-> Type` return annotation
            let ret = if matches!(self.peek(), Tok::Arrow) {
                self.advance();
                Some(self.parse_type_ann()?)
            } else {
                None
            };
            self.eat(&Tok::Eq, "before the function body")
                .map_err(|e| e.hint("a function body is an expression: `fn f(x) = x + 1`."))?;
            let body = self.expr()?;
            // ADR 0035: an optional `where` clause — the scaffolding AFTER the
            // point, scoped to this function. `where` stays an ordinary
            // identifier everywhere else (frames own a `.where(...)` verb), so
            // the gate is the exact shape `where <name> =` after the body,
            // optionally across newlines — no legal program parses that today.
            // Desugars to the `let … in` the body could have written: zero new
            // engine surface, parity by construction.
            let body = self.maybe_where_clause(body)?;
            return Ok(Stmt::Func {
                name,
                params,
                defaults,
                ret,
                exported,
                body,
                line: l,
                col: c,
            });
        }
        // `mut x = ...` or `mut a, b = ...`
        if matches!(self.peek(), Tok::Mut) {
            let (l, c) = self.pos();
            self.advance();
            let first = self.ident_name("after `mut`")?;
            if matches!(self.peek(), Tok::Comma) {
                let names = self.finish_target_list(first)?;
                self.eat(&Tok::Eq, "in destructuring assignment")?;
                let value = self.expr()?;
                return Ok(Stmt::Destructure { names, mutable: true, exported, value, line: l, col: c });
            }
            self.eat(&Tok::Eq, "in assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Assign {
                name: first,
                mutable: true,
                exported,
                value,
                line: l,
                col: c,
            });
        }
        // `a, b = ...` — destructuring (2+ names ending in `=`)
        if self.at_destructure() {
            let (l, c) = self.pos();
            let first = self.ident_name("")?;
            let names = self.finish_target_list(first)?;
            self.eat(&Tok::Eq, "in destructuring assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Destructure { names, mutable: false, exported, value, line: l, col: c });
        }
        // `x = ...`  (only when an identifier is immediately followed by a single `=`)
        if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Eq) {
            let (l, c) = self.pos();
            let name = self.ident_name("")?;
            self.eat(&Tok::Eq, "in assignment")?;
            let value = self.expr()?;
            return Ok(Stmt::Assign {
                name,
                mutable: false,
                exported,
                value,
                line: l,
                col: c,
            });
        }
        // A leading `export` consumed above but no definition followed — the user wrote
        // `export <expr>`; that's not a thing.
        if exported {
            let (l, c) = self.pos();
            return Err(HelixError::new("`export` must precede a definition", l, c).hint(
                "use `export fn …`, `export x = …`, or `export a, b = …`; only definitions are exported.",
            ));
        }
        Ok(Stmt::Expr(self.expr()?))
    }

    /// True if the upcoming tokens are `ident (, ident)+ =` — a destructuring
    /// target list (2+ names). A single `ident =` is a normal assignment.
    fn at_destructure(&self) -> bool {
        if !matches!(self.peek(), Tok::Ident(_)) {
            return false;
        }
        let mut k = self.pos + 1;
        let mut commas = 0;
        loop {
            match &self.toks[k].tok {
                Tok::Comma => {
                    commas += 1;
                    k += 1;
                    if !matches!(self.toks[k].tok, Tok::Ident(_)) {
                        return false;
                    }
                    k += 1;
                }
                Tok::Eq => return commas >= 1,
                _ => return false,
            }
        }
    }

    /// Given the first target name (already consumed), consume `, ident…`.
    fn finish_target_list(&mut self, first: String) -> Result<Vec<String>, HelixError> {
        let mut names = vec![first];
        while matches!(self.peek(), Tok::Comma) {
            self.advance();
            names.push(self.ident_name("as a destructuring target")?);
        }
        Ok(names)
    }

    fn ident_name(&mut self, ctx: &str) -> Result<String, HelixError> {
        match self.peek().clone() {
            Tok::Ident(n) => {
                self.advance();
                Ok(n)
            }
            other => {
                let (l, c) = self.pos();
                Err(HelixError::new(
                    format!("expected a name {}, found {}", ctx, other.describe()),
                    l,
                    c,
                ))
            }
        }
    }

    /// Like `ident_name`, but also accepts a reserved keyword as the name — used
    /// for member access after `.`, where a keyword can only be a member (so
    /// `python.import(...)`, `x.in`, etc. parse). Returns the keyword's source text.
    fn member_name(&mut self, ctx: &str) -> Result<String, HelixError> {
        let kw = match self.peek() {
            Tok::Ident(n) => Some(n.clone()),
            Tok::Mut => Some("mut".to_string()),
            Tok::Fn => Some("fn".to_string()),
            Tok::Import => Some("import".to_string()),
            Tok::And => Some("and".to_string()),
            Tok::Or => Some("or".to_string()),
            Tok::Not => Some("not".to_string()),
            Tok::If => Some("if".to_string()),
            Tok::Then => Some("then".to_string()),
            Tok::Else => Some("else".to_string()),
            Tok::Let => Some("let".to_string()),
            Tok::In => Some("in".to_string()),
            Tok::Match => Some("match".to_string()),
            Tok::Try => Some("try".to_string()),
            Tok::Do => Some("do".to_string()),
            Tok::Missing => Some("missing".to_string()),
            Tok::True => Some("true".to_string()),
            Tok::False => Some("false".to_string()),
            _ => None,
        };
        match kw {
            Some(n) => {
                self.advance();
                Ok(n)
            }
            None => {
                let (l, c) = self.pos();
                Err(HelixError::new(
                    format!("expected a name {}, found {}", ctx, self.peek().describe()),
                    l,
                    c,
                ))
            }
        }
    }

    /// Parse a function-signature type annotation: one capitalized type word.
    fn parse_type_ann(&mut self) -> Result<TypeAnn, HelixError> {
        let (l, c) = self.pos();
        let word = match self.peek().clone() {
            Tok::Ident(n) => n,
            other => {
                return Err(HelixError::new(
                    format!("expected a type name, found {}", other.describe()),
                    l,
                    c,
                )
                .hint("e.g. `fn area(w: Int, h: Int) -> Int = ...`."))
            }
        };
        let ann = match word.as_str() {
            "Int" => TypeAnn::Int,
            "Float" => TypeAnn::Float,
            "Num" => TypeAnn::Num,
            "String" => TypeAnn::String,
            "Bool" => TypeAnn::Bool,
            "Array" => TypeAnn::Array,
            "Tensor" => TypeAnn::Tensor,
            "DataFrame" => TypeAnn::DataFrame,
            "Dna" => TypeAnn::Dna,
            _ => {
                let mut err =
                    HelixError::new(format!("unknown type `{}`", word), l, c);
                if let Some(s) = suggest(&word, TYPE_NAMES) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("known types: {}", TYPE_NAMES.join(", ")));
                }
                return Err(err);
            }
        };
        self.advance();
        Ok(ann)
    }

    fn expr(&mut self) -> Result<Expr, HelixError> {
        if let Some(lam) = self.try_lambda()? {
            return Ok(lam);
        }
        self.coalesce_expr()
    }

    /// `a ?? b` — lowest precedence, left-associative.
    fn coalesce_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.or_expr()?;
        while matches!(self.peek(), Tok::Coalesce) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.or_expr()?;
            left = Expr::Binary {
                op: BinOp::Coalesce,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    /// Recognize `x => body` and `(a, b) => body` without consuming input unless
    /// it really is a lambda. Returns None (and leaves the cursor put) otherwise.
    fn try_lambda(&mut self) -> Result<Option<Expr>, HelixError> {
        // single parameter: IDENT =>
        if let Tok::Ident(name) = self.peek().clone() {
            if matches!(self.peek_at(1), Tok::FatArrow) {
                self.advance(); // IDENT
                self.advance(); // =>
                // A lambda body is the one expr() recursion that skips unary(),
                // so it must count a structural level itself — an unbounded
                // `x => x => …` chain would otherwise overflow the native stack.
                let saved = self.depth;
                self.deepen()?;
                let body = self.expr()?;
                self.depth = saved;
                return Ok(Some(Expr::Lambda {
                    params: vec![name],
                    body: Box::new(body),
                }));
            }
            return Ok(None);
        }

        // zero or more parameters: ( ) =>  or  ( IDENT (, IDENT)* ) =>
        if matches!(self.peek(), Tok::LParen) {
            let mut k = self.pos + 1;
            let mut params = Vec::new();
            // A zero-arg lambda `() => body` — a thunk (e.g. for benchmark harnesses).
            // Only commit if `=>` follows, so a bare `()` stays an ordinary expression.
            if matches!(self.toks[k].tok, Tok::RParen) {
                k += 1;
            } else {
                loop {
                    match &self.toks[k].tok {
                        Tok::Ident(nm) => {
                            params.push(nm.clone());
                            k += 1;
                            match &self.toks[k].tok {
                                Tok::Comma => {
                                    k += 1;
                                    continue;
                                }
                                Tok::RParen => {
                                    k += 1;
                                    break;
                                }
                                _ => return Ok(None),
                            }
                        }
                        _ => return Ok(None), // non-ident in param list — not a lambda
                    }
                }
            }
            if !matches!(self.toks[k].tok, Tok::FatArrow) {
                return Ok(None);
            }
            // Commit: consume `( params ) =>`, then parse the body.
            while self.pos <= k {
                self.advance();
            }
            // Same depth accounting as the single-param arm above.
            let saved = self.depth;
            self.deepen()?;
            let body = self.expr()?;
            self.depth = saved;
            return Ok(Some(Expr::Lambda {
                params,
                body: Box::new(body),
            }));
        }

        Ok(None)
    }

    fn or_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.and_expr()?;
        while matches!(self.peek(), Tok::Or) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.and_expr()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.equality()?;
        while matches!(self.peek(), Tok::And) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.equality()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.comparison()?;
        if matches!(self.peek(), Tok::EqEq | Tok::Ne) {
            let op = if matches!(self.peek(), Tok::EqEq) { BinOp::Eq } else { BinOp::Ne };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.comparison()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
            // A chained equality doesn't mean what it reads as — `1 == 1 == 1`
            // silently evaluated `(1 == 1) == 1` → `true == 1` → `false`.
            // Reject the chain (parentheses make the grouping explicit and are
            // still accepted), mirroring range chaining's rejection.
            if matches!(self.peek(), Tok::EqEq | Tok::Ne) {
                let (l2, c2) = self.pos();
                self.depth = saved;
                return Err(HelixError::new("comparisons cannot be chained", l2, c2)
                    .hint("split it with `and`, e.g. `a == b and b == c`."));
            }
        }
        self.depth = saved;
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_or()?;
        if let Some(op) = match self.peek() {
            Tok::Lt => Some(BinOp::Lt),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Le => Some(BinOp::Le),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        } {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_or()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
            // Same chain rejection as `equality` — `a < b < c` compared a Bool
            // to `c` (a type error at best, a silent wrong answer with bools).
            if matches!(self.peek(), Tok::Lt | Tok::Gt | Tok::Le | Tok::Ge) {
                let (l2, c2) = self.pos();
                self.depth = saved;
                return Err(HelixError::new("comparisons cannot be chained", l2, c2)
                    .hint("split it with `and`, e.g. `a < b and b < c`."));
            }
        }
        self.depth = saved;
        Ok(left)
    }

    // Bitwise operators (int-only), all left-associative. Mirroring Rust's ordering,
    // they bind tighter than comparison and looser than `+`/`-`, with precedence
    // `|` < `^` < `&` < `<<`/`>>`. So `mask >> j & 1 == 1` is `((mask>>j)&1)==1`.
    fn bit_or(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_xor()?;
        while matches!(self.peek(), Tok::Pipe) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_xor()?;
            left = Expr::Binary { op: BinOp::BitOr, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn bit_xor(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.bit_and()?;
        while matches!(self.peek(), Tok::Caret) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.bit_and()?;
            left = Expr::Binary { op: BinOp::BitXor, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn bit_and(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.shift()?;
        while matches!(self.peek(), Tok::Amp) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.shift()?;
            left = Expr::Binary { op: BinOp::BitAnd, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    fn shift(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.range_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Shl => BinOp::Shl,
                Tok::Shr => BinOp::Shr,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.range_expr()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right), line: l, col: c };
        }
        self.depth = saved;
        Ok(left)
    }

    /// A range literal `a..b` desugars to the existing `range(a, b)` builtin call,
    /// so every back-end (tree-walker, VM, JIT fusion via `as_range_call`) handles it
    /// with no further changes. `..` binds looser than `+`/`-` (so `0..n+1` is
    /// `0..(n+1)`) but tighter than comparisons; it does not chain (`a..b..c` is a
    /// parse error). Exclusive of the upper bound, matching `range`.
    fn range_expr(&mut self) -> Result<Expr, HelixError> {
        let left = self.term()?;
        if matches!(self.peek(), Tok::DotDot) {
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.term()?;
            if matches!(self.peek(), Tok::DotDot) {
                return Err(HelixError::new("a range `a..b` cannot be chained", l, c)
                    .hint("write a single `start..end`, e.g. `0..n`."));
            }
            return Ok(Expr::Call {
                name: "range".into(),
                args: vec![left, right],
                line: l,
                col: c,
            });
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.factor()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.factor()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    fn factor(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::SlashSlash => BinOp::FloorDiv,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            let (l, c) = self.pos();
            self.advance();
            self.deepen()?;
            let right = self.unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                line: l,
                col: c,
            };
        }
        self.depth = saved;
        Ok(left)
    }

    /// Account for one more structural level — recursive *nesting* (`((…))`) or
    /// left-spine growth from a *chain* (`a+b+c…`, `x.f().g()…`, built iteratively
    /// in the precedence loops below). Errors past `MAX_PARSE_DEPTH` so a later
    /// recursive pass over the AST — type-check, compile, or even `Box<Expr>`'s
    /// `Drop` — can't overflow the native stack on pathological input. The parser
    /// is the single place that bounds AST depth, so every depth-increasing
    /// construct must call this.
    fn deepen(&mut self) -> Result<(), HelixError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            let (l, c) = self.pos();
            return Err(
                HelixError::new("expression nested or chained too deeply", l, c)
                    .hint("split very large or deeply-nested expressions."),
            );
        }
        Ok(())
    }

    fn unary(&mut self) -> Result<Expr, HelixError> {
        // Every nesting path passes through here, so this bounds recursive-descent
        // depth; the precedence loops add `deepen()` to bound left-spine chains too.
        self.deepen()?;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        match self.peek() {
            Tok::Minus => {
                self.advance();
                // Was the operand written as an INTEGER whose magnitude overflowed i64?
                // Recorded before parsing, because the token is gone afterwards.
                let min_magnitude = matches!(self.peek(), Tok::BigInt(_, true));
                let e = self.unary()?;
                // Fold the sign into a BARE literal, the way the pattern parser already
                // does. This must happen AFTER the operand is parsed, not by peeking at
                // the next token: postfix binds tighter than unary minus, so `-1.abs()`
                // is `-(1.abs())`, and its operand does not come back as a bare literal.
                match e {
                    // A literal is always non-negative, so the negation cannot overflow.
                    Expr::Int(n) => Ok(Expr::Int(-n)),
                    // `9223372036854775808` does not fit an i64, but its NEGATION is
                    // exactly `i64::MIN`. Without this, the one integer that cannot be
                    // written positively could not be written at all: it degraded to an
                    // f64 and `-9223372036854775808` was a Float that merely printed like
                    // an integer. `big_int` is what keeps `-9223372036854775808.0`, an
                    // explicit float, a Float.
                    Expr::Float(_) if min_magnitude => Ok(Expr::Int(i64::MIN)),
                    Expr::Float(v) => Ok(Expr::Float(-v)),
                    other => Ok(Expr::Unary {
                        op: UnOp::Neg,
                        expr: Box::new(other),
                        line: l,
                        col: c,
                    }),
                }
            }
            Tok::Not => {
                self.advance();
                let e = self.unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                    line: l,
                    col: c,
                })
            }
            Tok::Try => {
                self.advance();
                let e = self.unary()?;
                Ok(Expr::Try { expr: Box::new(e), line: l, col: c })
            }
            _ => self.power(),
        }
    }

    /// Exponentiation binds tighter than unary minus and is right-associative,
    /// so `-2 ** 2` is `-(2 ** 2)` and `2 ** 3 ** 2` is `2 ** (3 ** 2)`.
    fn power(&mut self) -> Result<Expr, HelixError> {
        let base = self.postfix()?;
        if matches!(self.peek(), Tok::StarStar) {
            let (l, c) = self.pos();
            self.advance();
            let exp = self.unary()?;
            Ok(Expr::Binary {
                op: BinOp::Pow,
                left: Box::new(base),
                right: Box::new(exp),
                line: l,
                col: c,
            })
        } else {
            Ok(base)
        }
    }

    fn postfix(&mut self) -> Result<Expr, HelixError> {
        let saved = self.depth;
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.deepen()?;
                    let (l, c) = self.pos();
                    self.advance();
                    let name = self.member_name("after `.`")?;
                    // `.name(...)` is a method call; `.name` (no parens) is record
                    // field access — one obvious way: parens mean a call.
                    if matches!(self.peek(), Tok::LParen) {
                        self.advance();
                        let (args, named) = self.call_args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        // Named arguments are only meaningful on a *qualified module call*
                        // (`dep.f(a, k: v)`) — an ordinary `Method` here that the module
                        // loader resolves against the callee's signature. The sugar methods
                        // below take positional args only, so route a named call straight to
                        // the generic `Method` (carrying `named`); a named arg on a genuine
                        // method is then rejected by the checker, not the parser (the parser
                        // can't yet tell a module from a value).
                        if !named.is_empty() {
                            e = Expr::Method {
                                recv: Box::new(e),
                                name: name.clone(),
                                args,
                                named,
                                // A qualified module call (`dep.f(a, k: v)`): the loader
                                // rewrites the whole node into a resolved `Call`, so there
                                // is no fallback for it to carry.
                                ufcs: None,
                                line: l,
                                col: c,
                            };
                            continue;
                        }
                        // A method on an IMPORTED NAMESPACE is a qualified module call,
                        // never array sugar — left as a plain Method node for the module
                        // loader to resolve. The desugars below match by NAME with no
                        // idea what the receiver is, which intercepted
                        // `mechanics.position(x0, v, a, t)` at parse time and rejected a
                        // 4-arg module export with "takes one predicate function" — a
                        // false rejection (an arity-MATCHING call slipped past the check
                        // and resolved correctly, which is how the physics library found
                        // the seam). Seven names were affected: position, sort_by,
                        // take_while, drop_while, min_by, max_by, zipmap — and the list
                        // has since grown flat_map and count_where, which follow the
                        // same rule through this same gate.
                        if matches!(&e, Expr::Ident { name: n, .. } if self.imports.contains(n)) {
                            e = Expr::Method {
                                recv: Box::new(e),
                                name: name.clone(),
                                args,
                                named: vec![],
                                ufcs: None,
                                line: l,
                                col: c,
                            };
                            continue;
                        }
                        // `min_by`/`max_by`/`argmin`/`argmax` are sugar — desugared here
                        // into `map`/`enumerate` + `reduce` + index, so both engines get
                        // them for free (no new ops, parity by construction).
                        // UFCS IS DECIDED AT RUN TIME, BY THE RECEIVER (ADR 0045). This used
                        // to rewrite `x.f(a)` into `f(x, a)` here whenever `f` was a declared
                        // fn no type owned — a parse-time decision that was right for every
                        // program it could see and wrong for the one it could not: a record
                        // whose own field `f` holds a function could never win against a
                        // free `fn f`, and a PyObject receiver was invisible to the gate.
                        // Both engines now route a method call whose name is also a declared
                        // fn ON THE RECEIVER: a real method of its type, else a
                        // function-valued field, else the free fn with the receiver first —
                        // through the same entry a direct call takes, so `x.f(a)` costs what
                        // `f(x, a)` costs. A removed namespace (`stats.t_test(..)`) reports
                        // its migration hint from that route too. Where the checker PROVES the
                        // receiver's type and it rules the method reading out, a pass after the
                        // checker (src/ufcs.rs) makes the call a call before the compiler and
                        // the JIT see it — the same decision, made where it is cheapest and
                        // where the JIT can fuse it.
                        e = match name.as_str() {
                            "min_by" | "max_by" | "argmin" | "argmax" => {
                                desugar_order_by(e, &name, args, l, c)?
                            }
                            "sort_by" => desugar_sort_by(e, args, l, c)?,
                            "take_while" | "drop_while" => {
                                desugar_take_drop_while(e, &name, args, l, c)?
                            }
                            "position" => desugar_position(e, args, l, c)?,
                            "flat_map" | "count_where" => {
                                desugar_filter_compose(e, &name, args, l, c)?
                            }
                            // `a.zipmap(b, f)` == `a.zip(b).map(f)` — a paired
                            // elementwise map, desugared so both engines reuse the
                            // tested zip+map (parity by construction). For plain
                            // arithmetic, prefer broadcast (`a * b`, `sin(a) + b`).
                            "zipmap" => {
                                if args.len() != 2 {
                                    return Err(HelixError::new(
                                        format!("`zipmap` takes (other, fn), got {} arguments", args.len()),
                                        l,
                                        c,
                                    )
                                    .hint("e.g. `xs.zipmap(ys, (x, y) => x + y)`."));
                                }
                                let mut it = args.into_iter();
                                let other = it.next().unwrap();
                                let f = it.next().unwrap();
                                let zipped = Expr::Method {
                                    recv: Box::new(e),
                                    name: "zip".into(),
                                    args: vec![other],
                                    named: vec![],
                                    ufcs: None,
                                    line: l,
                                    col: c,
                                };
                                Expr::Method {
                                    recv: Box::new(zipped),
                                    name: "map".into(),
                                    args: vec![f],
                                    named: vec![],
                                    ufcs: None,
                                    line: l,
                                    col: c,
                                }
                            }
                            _ => Expr::Method {
                                recv: Box::new(e),
                                name: name.clone(),
                                args: wrap_bound_fn_arg(&name, args, l, c),
                                named: vec![],
                                ufcs: None,
                                line: l,
                                col: c,
                            },
                        };
                    } else {
                        e = Expr::Field {
                            recv: Box::new(e),
                            name,
                            line: l,
                            col: c,
                        };
                    }
                }
                Tok::LBracket => {
                    self.deepen()?;
                    let (l, c) = self.pos();
                    self.advance();
                    // `start? : stop? (: step?)?` is a slice; a bare expr is an index.
                    let start = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        let stop = if matches!(self.peek(), Tok::Colon | Tok::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        let step = if matches!(self.peek(), Tok::Colon) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBracket) {
                                None
                            } else {
                                Some(Box::new(self.expr()?))
                            }
                        } else {
                            None
                        };
                        self.eat(&Tok::RBracket, "to close the slice")?;
                        e = Expr::Slice {
                            recv: Box::new(e),
                            start,
                            stop,
                            step,
                            line: l,
                            col: c,
                        };
                    } else {
                        let index = match start {
                            Some(ix) => ix,
                            None => {
                                return Err(HelixError::new(
                                    "expected an index or slice inside `[...]`",
                                    l,
                                    c,
                                )
                                .hint("e.g. `xs[0]`, `xs[1:3]`, or `xs[::2]`."))
                            }
                        };
                        self.eat(&Tok::RBracket, "to close the index")?;
                        e = Expr::Index {
                            recv: Box::new(e),
                            index,
                            line: l,
                            col: c,
                        };
                    }
                }
                Tok::LParen => {
                    // A call is only valid directly on a bare name: `print(...)`.
                    if let Expr::Ident { name, line, col } = e {
                        self.deepen()?;
                        self.advance();
                        let (pos, named) = self.call_args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        let args = self.resolve_call_args(&name, pos, named, line, col)?;
                        e = Expr::Call {
                            name,
                            args,
                            line,
                            col,
                        };
                    } else {
                        // The call target is an expression, not a bare name:
                        // `(rec.handler)(x)`, `(fns[i])(x)`. Evaluate it to a function
                        // value and call it. Positional args only — named arguments
                        // bind to a declared function's parameter names, which a value
                        // call doesn't have visible.
                        let (l, c) = self.pos();
                        self.deepen()?;
                        self.advance();
                        let (pos, named) = self.call_args()?;
                        self.eat(&Tok::RParen, "to close the argument list")?;
                        if !named.is_empty() {
                            return Err(HelixError::new(
                                "a value call cannot use named arguments",
                                l,
                                c,
                            )
                            .hint("pass the arguments positionally, e.g. `(rec.handler)(x, y)`."));
                        }
                        e = Expr::CallValue {
                            callee: Box::new(e),
                            args: pos,
                            line: l,
                            col: c,
                        };
                    }
                }
                _ => break,
            }
        }
        self.depth = saved;
        Ok(e)
    }

    /// Parse a call's argument list into positional args and named args
    /// (`name: value`). Positional arguments must precede named ones.
    fn call_args(&mut self) -> Result<CallArgs, HelixError> {
        let mut pos = Vec::new();
        let mut named: Vec<(String, Expr)> = Vec::new();
        if matches!(self.peek(), Tok::RParen) {
            return Ok((pos, named));
        }
        loop {
            // `ident:` introduces a named argument (distinct from a bare expression).
            if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Colon) {
                let name = self.ident_name("as an argument name")?;
                self.advance(); // the `:`
                named.push((name, self.expr()?));
            } else {
                if !named.is_empty() {
                    let (l, c) = self.pos();
                    return Err(HelixError::new(
                        "a positional argument cannot follow a named one",
                        l,
                        c,
                    )
                    .hint("put positional arguments first, then named ones."));
                }
                pos.push(self.expr()?);
            }
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        Ok((pos, named))
    }

    /// Desugar a call's positional + named arguments into a single positional vector,
    /// using the called function's recorded signature: named arguments are placed by
    /// name and omitted parameters filled with their (literal) defaults. Calls to
    /// non-user functions (builtins) accept only positional arguments; their arity is
    /// checked later by the type checker, so positional args pass through unchanged.
    fn resolve_call_args(
        &self,
        name: &str,
        pos: Vec<Expr>,
        named: Vec<(String, Expr)>,
        line: usize,
        col: usize,
    ) -> Result<Vec<Expr>, HelixError> {
        let Some(sig) = self.fn_sigs.get(name) else {
            if !named.is_empty() {
                // A selectively-imported function IS user-defined — its parameter
                // names just live in another file, which this parser cannot see.
                if self.selected_imports.contains(name) {
                    return Err(HelixError::new(
                        format!(
                            "`{name}` was imported selectively, so its parameter \
                             names are not visible here"
                        ),
                        line,
                        col,
                    )
                    .hint(format!(
                        "import the module under its name and call `m.{name}(...)` \
                         qualified — qualified calls support named arguments — or \
                         pass the arguments positionally."
                    )));
                }
                return Err(HelixError::new(
                    format!("named arguments are only supported for user-defined functions, not `{name}`"),
                    line,
                    col,
                )
                .hint("pass arguments to builtins positionally."));
            }
            return Ok(pos);
        };
        let n = sig.params.len();
        // Fast path: plain positional call that the arity check can handle directly.
        if named.is_empty() && (pos.len() == n || sig.defaults.iter().all(|d| d.is_none())) {
            return Ok(pos);
        }
        if pos.len() > n {
            return Err(HelixError::new(
                format!(
                    "`{name}` takes {n} parameter{}, but {} positional arguments were given",
                    if n == 1 { "" } else { "s" },
                    pos.len()
                ),
                line,
                col,
            ));
        }
        let mut slots: Vec<Option<Expr>> = (0..n).map(|_| None).collect();
        for (i, p) in pos.into_iter().enumerate() {
            slots[i] = Some(p);
        }
        for (pname, value) in named {
            let Some(idx) = sig.params.iter().position(|p| *p == pname) else {
                return Err(HelixError::new(
                    format!("`{name}` has no parameter named `{pname}`"),
                    line,
                    col,
                )
                .hint(format!("its parameters are: {}", sig.params.join(", "))));
            };
            if slots[idx].is_some() {
                return Err(HelixError::new(
                    format!("parameter `{pname}` of `{name}` was given more than once"),
                    line,
                    col,
                ));
            }
            slots[idx] = Some(value);
        }
        let mut out = Vec::with_capacity(n);
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(e) => out.push(e),
                None => match &sig.defaults[i] {
                    Some(d) => out.push(d.clone()),
                    None => {
                        return Err(HelixError::new(
                            format!("`{name}` is missing an argument for parameter `{}`", sig.params[i]),
                            line,
                            col,
                        )
                        .hint("pass it positionally or by name, or give the parameter a default."))
                    }
                },
            }
        }
        Ok(out)
    }

    /// Parse a pattern, collecting `a | b | c` alternatives into an `Or`. Nested
    /// tuple/record patterns recurse back through here, so this is the one chokepoint
    /// that bounds pattern-nesting depth — without it, `match x { (((…))) => … }`
    /// overflows the native stack (patterns don't pass through `unary`).
    fn parse_pattern(&mut self) -> Result<crate::ast::Pattern, HelixError> {
        self.deepen()?;
        let r = self.parse_pattern_alts();
        self.depth -= 1;
        r
    }

    fn parse_pattern_alts(&mut self) -> Result<crate::ast::Pattern, HelixError> {
        let (l, c) = self.pos();
        let first = self.parse_single_pattern()?;
        if !matches!(self.peek(), Tok::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while matches!(self.peek(), Tok::Pipe) {
            self.advance();
            alts.push(self.parse_single_pattern()?);
        }
        // v1: or-pattern alternatives must not bind variables, so the arm's bindings
        // are unambiguous regardless of which alternative matched.
        if alts.iter().any(|a| !crate::interp::pattern_binding_names(a).is_empty()) {
            return Err(HelixError::new("or-patterns (`a | b`) cannot bind variables yet", l, c)
                .hint("use literal alternatives, e.g. `1 | 2 | 3 => ...`."));
        }
        Ok(crate::ast::Pattern::Or(alts))
    }

    /// A numeric literal pattern that may continue into a RANGE (`200..300`). The
    /// literal has already been consumed; `plain` is the pattern to return when no
    /// `..` follows, and `lo` is that literal's value.
    ///
    /// The bounds are checked here rather than at match time because both are known
    /// now and a bad one can never match anything: an empty or reversed interval is a
    /// typo, not a pattern that happens never to fire, and saying so at the point it
    /// was written costs nothing.
    fn maybe_range_pattern(
        &mut self,
        plain: crate::ast::Pattern,
        lo: f64,
        l: usize,
        c: usize,
    ) -> Result<crate::ast::Pattern, HelixError> {
        if !matches!(self.peek(), Tok::DotDot) {
            return Ok(plain);
        }
        self.advance();
        let hi = self.parse_range_bound(l, c)?;
        exact_bound(lo, l, c)?;
        exact_bound(hi, l, c)?;
        // NaN is already excluded by `exact_bound` above, so the direct comparison
        // says what it means: an interval that cannot contain anything.
        if lo >= hi {
            return Err(HelixError::new(
                format!("a range pattern needs a low bound below its high bound, got `{lo}..{hi}`"),
                l,
                c,
            )
            .hint(
                "`lo..hi` matches lo up to but NOT including hi, so an empty or \
                 reversed range can never match — did you mean the bounds the other \
                 way round?",
            ));
        }
        Ok(crate::ast::Pattern::Range { lo, hi })
    }

    /// The upper bound of a range pattern: a number, optionally negative.
    fn parse_range_bound(&mut self, l: usize, c: usize) -> Result<f64, HelixError> {
        let neg = if matches!(self.peek(), Tok::Minus) {
            self.advance();
            true
        } else {
            false
        };
        let v = match self.peek().clone() {
            Tok::Int(v) => {
                self.advance();
                v as f64
            }
            Tok::Float(v) | Tok::BigInt(v, _) => {
                self.advance();
                v
            }
            other => {
                return Err(HelixError::new(
                    format!("expected a number after `..` in a range pattern, found {}", other.describe()),
                    l,
                    c,
                )
                .hint("a range pattern spans two numbers, e.g. `200..300 => \"success\"`."))
            }
        };
        Ok(if neg { -v } else { v })
    }

    /// Parse one pattern alternative: a literal, `_`, a name to bind, a tuple, or a
    /// record. Tuple elements and record field values recurse through `parse_pattern`,
    /// so `|` nests inside them.
    fn parse_single_pattern(&mut self) -> Result<crate::ast::Pattern, HelixError> {
        use crate::ast::Pattern;
        let (l, c) = self.pos();
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok(if name == "_" { Pattern::Wildcard } else { Pattern::Bind(name) })
            }
            Tok::Int(v) => {
                self.advance();
                self.maybe_range_pattern(Pattern::Int(v), v as f64, l, c)
            }
            Tok::Float(v) | Tok::BigInt(v, _) => {
                self.advance();
                self.maybe_range_pattern(Pattern::Float(v), v, l, c)
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Pattern::Str(s))
            }
            Tok::True => {
                self.advance();
                Ok(Pattern::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Pattern::Bool(false))
            }
            Tok::Missing => {
                self.advance();
                Ok(Pattern::Missing)
            }
            Tok::Minus => {
                self.advance();
                match self.peek().clone() {
                    Tok::Int(v) => {
                        self.advance();
                        self.maybe_range_pattern(Pattern::Int(-v), -v as f64, l, c)
                    }
                    Tok::BigInt(_, true) => {
                        self.advance();
                        self.maybe_range_pattern(Pattern::Int(i64::MIN), i64::MIN as f64, l, c)
                    }
                    Tok::Float(v) | Tok::BigInt(v, _) => {
                        self.advance();
                        self.maybe_range_pattern(Pattern::Float(-v), -v, l, c)
                    }
                    other => Err(HelixError::new(
                        format!("expected a number after `-` in a pattern, found {}", other.describe()),
                        l,
                        c,
                    )),
                }
            }
            Tok::LParen => {
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    self.advance();
                    return Ok(Pattern::Tuple(Vec::new()));
                }
                let first = self.parse_pattern()?;
                if matches!(self.peek(), Tok::Comma) {
                    let mut pats = vec![first];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break; // trailing comma / `(p,)`
                        }
                        pats.push(self.parse_pattern()?);
                    }
                    self.eat(&Tok::RParen, "to close a tuple pattern")?;
                    Ok(Pattern::Tuple(pats))
                } else {
                    self.eat(&Tok::RParen, "to close a grouped pattern")?;
                    Ok(first) // `(p)` just groups
                }
            }
            Tok::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Tok::RBrace) {
                    let (kl, kc) = self.pos();
                    let key = self.ident_name("as a record-pattern field")?;
                    // Same rule as a record literal: one entry per field (a
                    // duplicate here could only re-test or re-bind the same
                    // field — always a mistake).
                    if fields.iter().any(|(k, _): &(String, Pattern)| k == &key) {
                        return Err(HelixError::new(
                            format!("duplicate field `{}` in record pattern", key),
                            kl,
                            kc,
                        )
                        .hint("each field may appear once in a pattern."));
                    }
                    let subpat = if matches!(self.peek(), Tok::Colon) {
                        self.advance();
                        self.parse_pattern()?
                    } else {
                        Pattern::Bind(key.clone()) // `{field}` shorthand binds the field
                    };
                    fields.push((key, subpat));
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace, "to close a record pattern")?;
                Ok(Pattern::Record(fields))
            }
            other => Err(HelixError::new(
                format!("expected a pattern, found {}", other.describe()),
                l,
                c,
            )
            .hint("a pattern is a literal, `_`, a name, a tuple `(a, b)`, or a record `{ok: true, value: v}`.")),
        }
    }

    /// `do { name = expr  <newline>  ...  final_expr }` — a block of sequential
    /// bindings ending in a result expression. Desugars to `let name = expr, … in
    /// final_expr`, so every back-end and the checker handle it with no changes;
    /// with no bindings it is just the body. Bindings are newline-separated (Helix
    /// has no `;`), and the last non-binding line is the block's value. The bindings
    /// are immutable `let`s, like `let … in`, so the language model is unchanged —
    /// `do` only flattens deep `let … in` chains so they read top-to-bottom.
    fn do_block(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        self.advance(); // `do`
        self.eat(&Tok::LBrace, "after `do`")
            .map_err(|e| e.hint("a `do` block looks like `do { x = 1\\n  y = 2\\n  x + y }`."))?;
        self.skip_newlines();
        let mut bindings = Vec::new();
        loop {
            if matches!(self.peek(), Tok::Mut) {
                let (ml, mc) = self.pos();
                return Err(mut_inside_a_body(ml, mc));
            }
            // `fn` is item-level only. Without this arm the catch-all says "expected a
            // value here" — true and useless, because the author's mistake is a RULE
            // they cannot see, not a typo. Name the rule and show the local form.
            if matches!(self.peek(), Tok::Fn) {
                let (fl, fc) = self.pos();
                return Err(HelixError::new(
                    "`fn` cannot be defined inside a `do` block",
                    fl,
                    fc,
                )
                .hint(
                    "`fn` is item-level — define it at the top of the file. For a local \
                     function, bind a lambda instead: `f = (x) => x * 2`.",
                ));
            }
            // A binding is `IDENT = expr` — a single `=`, never `==`. Anything else
            // is the block's final result expression.
            let binding_name = match self.peek().clone() {
                Tok::Ident(name) if matches!(self.peek_at(1), Tok::Eq) => Some(name),
                _ => None,
            };
            if let Some(name) = binding_name {
                let (bl, bc) = self.pos();
                self.advance(); // IDENT
                self.advance(); // =
                let value = self.expr()?;
                self.do_bindings.push((name.clone(), bl, bc));
                bindings.push((name, value));
                self.skip_newlines();
                continue;
            }
            if matches!(self.peek(), Tok::RBrace) {
                return Err(HelixError::new("a `do` block must end with a result expression", l, c)
                    .hint("add a final line that produces the block's value, e.g. `do { x = 1\\n  x + 1 }`."));
            }
            // `do { x = e in … }` mixes the two binding forms — the giveaway that the
            // author reached for `let … in` muscle memory inside a block. Name the exact
            // mistake instead of a generic "unexpected `in`".
            if matches!(self.peek(), Tok::In) {
                let (il, ic) = self.pos();
                return Err(HelixError::new("unexpected `in` inside a `do` block", il, ic).hint(
                    "`do {}` and `let … in …` are different binding forms. A `do` block \
                     separates statements by newlines and has no `in` — either drop the `in` \
                     (`do { x = e\\n  result }`), or use `let x = e in result` without the `do {}`.",
                ));
            }
            let expr = self.expr()?;
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace) {
                // The last line is the block's result expression.
                self.advance(); // `}`
                return Ok(if bindings.is_empty() {
                    expr
                } else {
                    Expr::Let { bindings, body: Box::new(expr), from_do: true }
                });
            }
            // A non-final bare expression is a side-effecting statement (e.g. `print(…)`).
            // Bind it to a fresh throwaway name (`$do<N>` — `$` can't appear in user code,
            // so it never collides or shadows) so it's evaluated for its effect, then
            // continue to the next statement. This is the hand-written `p1 = print(…)`
            // idiom, done by the parser — a `do` block reads as a true statement sequence.
            bindings.push((format!("$do{}", bindings.len()), expr));
        }
    }

    /// Stamp every positioned node of a parsed interpolation-hole fragment with
    /// the interpolated string's real source position. Fragments are lexed and
    /// parsed as standalone snippets, so their nodes carry snippet-relative
    /// positions (line 1, column within the hole); this makes both engines'
    /// runtime errors point at the interpolated string itself. Exhaustive on
    /// purpose — a new `Expr` variant must decide its treatment here.
    fn relocate(e: &mut Expr, l: usize, c: usize) {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => {}
            Expr::Interp(parts) => {
                for p in parts.iter_mut() {
                    if let InterpPart::Expr(inner, _) = p {
                        Self::relocate(inner, l, c);
                    }
                }
            }
            Expr::Ident { line, col, .. } | Expr::Column { line, col, .. } => {
                *line = l;
                *col = c;
            }
            Expr::Array(items) | Expr::Tuple(items) => {
                for i in items {
                    Self::relocate(i, l, c);
                }
            }
            Expr::Record(fields) => {
                for (_, v) in fields {
                    Self::relocate(v, l, c);
                }
            }
            Expr::RecordUpdate { base, fields, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(base, l, c);
                for (_, v) in fields {
                    Self::relocate(v, l, c);
                }
            }
            Expr::Field { recv, line, col, .. } => {
                *line = l;
                *col = c;
                Self::relocate(recv, l, c);
            }
            Expr::Unary { expr, line, col, .. } => {
                *line = l;
                *col = c;
                Self::relocate(expr, l, c);
            }
            Expr::Binary { left, right, line, col, .. } => {
                *line = l;
                *col = c;
                Self::relocate(left, l, c);
                Self::relocate(right, l, c);
            }
            Expr::Call { args, line, col, .. } => {
                *line = l;
                *col = c;
                for a in args {
                    Self::relocate(a, l, c);
                }
            }
            Expr::Method { recv, args, named, line, col, .. } => {
                *line = l;
                *col = c;
                Self::relocate(recv, l, c);
                for a in args {
                    Self::relocate(a, l, c);
                }
                for (_, v) in named {
                    Self::relocate(v, l, c);
                }
            }
            Expr::CallValue { callee, args, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(callee, l, c);
                for a in args {
                    Self::relocate(a, l, c);
                }
            }
            Expr::Index { recv, index, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(recv, l, c);
                Self::relocate(index, l, c);
            }
            Expr::Slice { recv, start, stop, step, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(recv, l, c);
                for o in [start, stop, step].into_iter().flatten() {
                    Self::relocate(o, l, c);
                }
            }
            Expr::Lambda { body, .. } => Self::relocate(body, l, c),
            Expr::Let { bindings, body, .. } => {
                for (_, v) in bindings {
                    Self::relocate(v, l, c);
                }
                Self::relocate(body, l, c);
            }
            Expr::If { cond, then_branch, else_branch, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(cond, l, c);
                Self::relocate(then_branch, l, c);
                Self::relocate(else_branch, l, c);
            }
            Expr::Try { expr, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(expr, l, c);
            }
            Expr::Match { scrutinee, arms, line, col } => {
                *line = l;
                *col = c;
                Self::relocate(scrutinee, l, c);
                for a in arms {
                    if let Some(g) = &mut a.guard {
                        Self::relocate(g, l, c);
                    }
                    Self::relocate(&mut a.body, l, c);
                }
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, HelixError> {
        let (l, c) = self.pos();
        match self.peek().clone() {
            Tok::Int(v) => {
                self.advance();
                Ok(Expr::Int(v))
            }
            Tok::Float(v) | Tok::BigInt(v, _) => {
                self.advance();
                Ok(Expr::Float(v))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::InterpStr(segs) => {
                self.advance();
                let mut parts = Vec::with_capacity(segs.len());
                for seg in segs {
                    match seg {
                        StrSeg::Lit(t) => parts.push(InterpPart::Lit(t)),
                        StrSeg::Expr(src, spec_src) => {
                            // The fragment is lexed+parsed as its own snippet, so any
                            // error inside it carries snippet-relative positions (line 1).
                            // Relocate it to the interpolated string's real position so
                            // the caret points at the user's actual source, not line 1.
                            let (mut e, hole_do_bindings) =
                                parse_expression(&src, self.depth, &self.fn_sigs, &self.imports, &self.fn_names)
                                    .map_err(|err| HelixError { line: l, col: c, ..err })?;
                            // Fold the hole's `do {}` bindings into this parser's list, at
                            // the string's real position for the same reason the error and
                            // the AST below are relocated: the snippet's own positions are
                            // line-1 relative and would point at unrelated source.
                            self.do_bindings
                                .extend(hole_do_bindings.into_iter().map(|(n, _, _)| (n, l, c)));
                            // Relocate the *retained AST* too, not just the parse error:
                            // a RUNTIME error inside a hole (div-by-zero, format-spec
                            // mismatch, missing method) reports the hole expression's
                            // position, which would otherwise be the snippet's line 1 —
                            // a caret into unrelated early source, and a walker/VM
                            // position mismatch.
                            Self::relocate(&mut e, l, c);
                            // Parse the format spec now, so a malformed spec is a parse
                            // error pointing at the string (never a runtime surprise).
                            let spec = match spec_src {
                                Some(sp) => Some(
                                    // A failed spec is often not a spec at all:
                                    // `".a{color:red}"` is CSS whose brace was read
                                    // as a hole (`:red` as the spec). Name the
                                    // escape hatch every time.
                                    crate::strfmt::parse_spec(&sp).map_err(|m| {
                                        HelixError::new(m, l, c).hint(
                                            "a `{...}` hole interpolates; for literal \
                                             braces (CSS, JSON) write `{{` and `}}` — \
                                             `\".a{{color:red}}\"` prints `.a{color:red}`.",
                                        )
                                    })?,
                                ),
                                None => None,
                            };
                            parts.push(InterpPart::Expr(Box::new(e), spec));
                        }
                    }
                }
                Ok(Expr::Interp(parts))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::Missing => {
                self.advance();
                Ok(Expr::Missing)
            }
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr::Ident { name, line: l, col: c })
            }
            Tok::At => {
                // `@name` — a DataFrame column reference. The name that follows must
                // be a plain identifier (`@age`), not a keyword or expression.
                self.advance();
                let name = self.ident_name("after `@` (a column name)").map_err(|e| {
                    e.hint("`@` marks a DataFrame column, e.g. `df.where(@age > 40)`.")
                })?;
                Ok(Expr::Column { name, line: l, col: c })
            }
            Tok::Let => {
                self.advance();
                let mut bindings = Vec::new();
                loop {
                    if matches!(self.peek(), Tok::Mut) {
                        let (ml, mc) = self.pos();
                        return Err(mut_inside_a_body(ml, mc));
                    }
                    let name = self.ident_name("as a `let` binding")?;
                    self.eat(&Tok::Eq, "in a `let` binding")
                        .map_err(|e| e.hint("`let` looks like `let a = 1, b = 2 in a + b`."))?;
                    let value = self.expr()?;
                    bindings.push((name, value));
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::In, "after `let` bindings")
                    .map_err(|e| e.hint("a `let` needs an `in` body: `let x = 1 in x + 1`."))?;
                let body = self.expr()?;
                Ok(Expr::Let {
                    bindings,
                    body: Box::new(body),
                    from_do: false,
                })
            }
            Tok::Do => self.do_block(),
            Tok::If => {
                self.advance();
                let cond = self.expr()?;
                let saw_eq = matches!(self.peek(), Tok::Eq);
                self.eat(&Tok::Then, "after the condition").map_err(|e| {
                    if saw_eq {
                        // `if x = 5` — the classic `=` (assignment) vs `==` (equality) slip.
                        e.hint("did you mean `==`? A single `=` is assignment; conditions test with `==`.")
                    } else {
                        e.hint("an `if` expression looks like `if cond then a else b`.")
                    }
                })?;
                let then_branch = self.expr()?;
                self.eat(&Tok::Else, "after the `then` branch").map_err(|e| {
                    e.hint("`if` is an expression, so it always needs an `else` branch that yields a value.")
                })?;
                let else_branch = self.expr()?;
                Ok(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                    line: l,
                    col: c,
                })
            }
            Tok::Match => {
                self.advance();
                let scrutinee = self.expr()?;
                self.eat(&Tok::LBrace, "after the `match` value").map_err(|e| {
                    e.hint("a `match` looks like `match x { 0 => \"zero\", _ => \"other\" }`.")
                })?;
                let mut arms = Vec::new();
                while !matches!(self.peek(), Tok::RBrace) {
                    let pattern = self.parse_pattern()?;
                    // Optional guard: `pat if cond => ...`.
                    let guard = if matches!(self.peek(), Tok::If) {
                        self.advance();
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    self.eat(&Tok::FatArrow, "after a match pattern").map_err(|e| {
                        e.hint("each arm is `pattern [if guard] => result`, e.g. `0 => \"zero\"`.")
                    })?;
                    let body = self.expr()?;
                    arms.push(crate::ast::MatchArm { pattern, guard, body });
                    if matches!(self.peek(), Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.eat(&Tok::RBrace, "to close the `match`")?;
                if arms.is_empty() {
                    return Err(HelixError::new("a `match` needs at least one arm", l, c)
                        .hint("e.g. `match x { 0 => \"zero\", _ => \"other\" }`."));
                }
                Ok(Expr::Match { scrutinee: Box::new(scrutinee), arms, line: l, col: c })
            }
            Tok::LParen => {
                // `(e)` groups; `()` / `(a, b)` / `(x,)` build a tuple. (A lambda
                // `(a, b) =>` was already handled by `try_lambda`.)
                self.advance();
                if matches!(self.peek(), Tok::RParen) {
                    self.advance();
                    return Ok(Expr::Tuple(Vec::new()));
                }
                let first = self.expr()?;
                if matches!(self.peek(), Tok::Comma) {
                    let mut elems = vec![first];
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RParen) {
                            break; // trailing comma / `(x,)`
                        }
                        elems.push(self.expr()?);
                    }
                    self.eat(&Tok::RParen, "to close the tuple")?;
                    Ok(Expr::Tuple(elems))
                } else {
                    self.eat(&Tok::RParen, "to close the group")?;
                    Ok(first)
                }
            }
            Tok::LBracket => {
                self.advance();
                let mut elems = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        elems.push(self.expr()?);
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBracket) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                // A LIST COMPREHENSION is the shape that lands here: `[x * 2 for x in xs]`
                // parses `x * 2`, then finds `for` where a `,` or `]` belongs. It is the one
                // Python habit with an exact Helix translation, so say it rather than
                // reporting a bracket.
                if let Tok::Ident(w) = self.peek()
                    && matches!(w.as_str(), "for" | "if")
                {
                    let (l, c) = self.pos();
                    return Err(HelixError::new(
                        format!("expected `]` to close the array, found `{w}`"),
                        l,
                        c,
                    )
                    .hint(
                        "Helix has no list comprehension — `[f(x) for x in xs]` is `xs.map(x => f(x))`, \
                         and a trailing `if` is `.filter(...)` before it.",
                    ));
                }
                self.eat(&Tok::RBracket, "to close the array")?;
                Ok(Expr::Array(elems))
            }
            Tok::LBrace => {
                // record literal: `{ name: expr, age: expr }`, or a record UPDATE that
                // starts with a spread: `{ ...base, status: 500 }`.
                let (l, c) = self.pos();
                self.advance();
                // A leading `...expr` makes this a record update: clone `expr`, then apply the
                // fields that follow. The spread must come first (there is one base).
                if matches!(self.peek(), Tok::DotDotDot) {
                    self.advance();
                    let base = self.expr()?;
                    let mut fields: Vec<(String, Expr)> = Vec::new();
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RBrace) {
                            break; // trailing comma
                        }
                        // A SECOND spread lands here, and used to report "expected a name
                        // as a record field name, found `...`" — which describes the token
                        // rather than the problem. One base, so `{...a, ...b}` has no
                        // meaning to give it.
                        if matches!(self.peek(), Tok::DotDotDot) {
                            let (sl, sc) = self.pos();
                            return Err(HelixError::new(
                                "a record update takes one `...spread`, not two",
                                sl,
                                sc,
                            )
                            .hint(
                                "`{ ...base, field: value }` updates ONE record; there is no \
                                 merge form, so name the fields you want from the second.",
                            ));
                        }
                        let (kl, kc) = self.pos();
                        let key = self.member_name("as a record field name")?;
                        // The same duplicate-field rejection the plain record literal does,
                        // for the same reason: order-independent equality assumes one entry
                        // per key, so two "equal" records could otherwise disagree on `.a`.
                        // This branch was missing it, so `{y: 2, y: 3}` was a parse error
                        // while `{...b, y: 2, y: 3}` was silently accepted with last-wins —
                        // one mistake, caught in one spelling and not the other.
                        //
                        // Overriding a field that came from the BASE is untouched: that is
                        // the entire purpose of an update, and `{...b, y: 9}` where `b` has
                        // a `y` stays legal. Only a repeat within THIS field list is a
                        // duplicate.
                        if fields.iter().any(|(k, _)| k == &key) {
                            return Err(HelixError::new(
                                format!("duplicate field `{}` in record update", key),
                                kl,
                                kc,
                            )
                            .hint("each field may be given once; the later value would silently win."));
                        }
                        self.eat(&Tok::Colon, &format!("after field `{}`", key))?;
                        fields.push((key, self.expr()?));
                    }
                    self.eat(&Tok::RBrace, "to close the record update")?;
                    return Ok(Expr::RecordUpdate { base: Box::new(base), fields, line: l, col: c });
                }
                let mut fields: Vec<(String, Expr)> = Vec::new();
                // A QUOTED key makes this a DICT literal rather than a record: a record
                // field is a name, and the maps people actually write — HTTP headers,
                // JSON objects, lookup tables transcribed from a document — have keys
                // that are not names. `{"Content-Type": "text/html"}` had no spelling
                // at all before this; it was `[("Content-Type", "text/html")].to_dict()`,
                // a constructor shaped like a fold.
                //
                // Desugars to that same `.to_dict()` call, so the engines and the
                // checker need to know nothing about it.
                if matches!(self.peek(), Tok::Str(_)) {
                    let mut entries: Vec<Expr> = Vec::new();
                    let mut seen: Vec<String> = Vec::new();
                    loop {
                        let (kl, kc) = self.pos();
                        let key = match self.peek().clone() {
                            Tok::Str(k) => {
                                self.advance();
                                k
                            }
                            other => {
                                return Err(HelixError::new(
                                    format!(
                                        "expected a quoted key in this dict, found {}",
                                        other.describe()
                                    ),
                                    kl,
                                    kc,
                                )
                                .hint(
                                    "a brace with quoted keys is a dict — every key must be \
                                     quoted. For a record, use bare names: `{name: \"Ada\"}`.",
                                ))
                            }
                        };
                        // A duplicate is a typo you can see, so it is refused here rather
                        // than silently resolved last-wins as `to_dict` would.
                        if seen.contains(&key) {
                            return Err(HelixError::new(
                                format!("duplicate key `{}` in dict literal", key),
                                kl,
                                kc,
                            )
                            .hint("each key may appear once; add or replace one with `d.insert(k, v)`."));
                        }
                        seen.push(key.clone());
                        self.eat(&Tok::Colon, &format!("after key `{}`", key)).map_err(|e| {
                            e.hint("dicts look like `{\"a\": 1, \"b\": 2}`.")
                        })?;
                        let value = self.expr()?;
                        entries.push(Expr::Tuple(vec![Expr::Str(key), value]));
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                    self.eat(&Tok::RBrace, "to close the dict")?;
                    return Ok(Expr::Method {
                        recv: Box::new(Expr::Array(entries)),
                        name: "to_dict".to_string(),
                        args: vec![],
                        named: vec![],
                        ufcs: None,
                        line: l,
                        col: c,
                    });
                }
                if !matches!(self.peek(), Tok::RBrace) {
                    loop {
                        // A spread is only valid as the FIRST element (there is one base).
                        if matches!(self.peek(), Tok::DotDotDot) {
                            let (sl, sc) = self.pos();
                            return Err(HelixError::new(
                                "a `...spread` must be the first element of a record update",
                                sl,
                                sc,
                            )
                            .hint("write `{ ...base, field: value }` — the spread comes first."));
                        }
                        // Field names may be keywords (`match`, `in`, `if`, …) — they
                        // are contextual here, never ambiguous before a `:`.
                        let (kl, kc) = self.pos();
                        // A quoted key here means the two brace forms got mixed: the
                        // first key was a bare name, so this is a record, and a record
                        // field is a name. Say what the two forms are — the reader is
                        // one keystroke from either.
                        if matches!(self.peek(), Tok::Str(_)) {
                            return Err(HelixError::new(
                                "this brace began as a record, so its keys must be bare names",
                                kl,
                                kc,
                            )
                            .hint(
                                "a brace is one of two things: a RECORD with bare-name fields \
                                 (`{name: \"Ada\"}`), or a DICT with quoted keys \
                                 (`{\"Content-Type\": \"text/html\"}`). Quote every key, or none.",
                            ));
                        }
                        let key = self.member_name("as a record field name")?;
                        // A duplicate field would break `==`'s substitutability
                        // (order-independent equality assumes one entry per key:
                        // two "equal" records could disagree on `.a`), so reject
                        // it here — the one place all engines share.
                        if fields.iter().any(|(k, _)| k == &key) {
                            return Err(HelixError::new(
                                format!("duplicate field `{}` in record literal", key),
                                kl,
                                kc,
                            )
                            .hint("each field may appear once; derive a changed record with `{ ...base, field: value }`."));
                        }
                        self.eat(&Tok::Colon, &format!("after field `{}`", key))
                            .map_err(|e| {
                                e.hint("records look like `{name: \"Ada\", age: 41}`.")
                            })?;
                        let value = self.expr()?;
                        fields.push((key, value));
                        if matches!(self.peek(), Tok::Comma) {
                            self.advance();
                            if matches!(self.peek(), Tok::RBrace) {
                                break; // trailing comma
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.eat(&Tok::RBrace, "to close the record")?;
                Ok(Expr::Record(fields))
            }
            other => Err(HelixError::new(
                format!("unexpected {}", other.describe()),
                l,
                c,
            )
            .hint("expected a value here — a number, string, name, or `[...]` array.")),
        }
    }
}

/// A range bound must be a number `f64` holds exactly. Beyond 2^53 consecutive
/// integers stop being representable, so a bound written there would silently match a
/// different interval than the one on the page — rare, and a wrong answer if reached.
fn exact_bound(v: f64, l: usize, c: usize) -> Result<(), HelixError> {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if v.is_nan() || v.abs() > EXACT {
        return Err(HelixError::new(
            format!("`{v}` is too large to be an exact range bound"),
            l,
            c,
        )
        .hint("range-pattern bounds are held as f64; use a guard (`n if n > …`) for magnitudes beyond 2^53."));
    }
    Ok(())
}
