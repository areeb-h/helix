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

use crate::ast::{ImportNames, BinOp, Expr, InterpPart, Stmt, TypeAnn, UnOp};
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

/// A parsed parameter list: `(name, annotation)` pairs and their parallel defaults.
type Params = (Vec<(String, Option<TypeAnn>)>, Vec<Option<Expr>>);

/// True for the literal-constant expressions allowed as a parameter default. The
/// default is inserted at each call site, so it must not reference anything (no
/// params, no globals) — a literal (optionally negated/notted) guarantees that.
fn is_const_default(e: &Expr) -> bool {
    match e {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Missing => true,
        Expr::Unary { expr, .. } => is_const_default(expr),
        // A literal container of literals is a literal: `= {}`, `= []`, `= (1, 2)`,
        // `= {retries: 3}`. An options record could not have an empty default before
        // (field build), and there was no reading under which `{}` is not a constant.
        Expr::Array(items) | Expr::Tuple(items) => items.iter().all(is_const_default),
        Expr::Record(fields) => fields.iter().all(|(_, v)| is_const_default(v)),
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
fn parse_expression(src: &str, outer: &Parser) -> Result<Hole, HelixError> {
    parse_expression_at(src, (1, 1, false), outer)
}

/// What a hole's own parser hands back to the one that holds the string: the expression,
/// the `do {}` binding names in it, and how many `for` loops it held (ADR 0066: a function
/// with a `where` clause reads its body again if the body has a loop).
type Hole = (Expr, Vec<DoBinding>, usize, Vec<GoneUse>);

/// [`parse_expression`] for source that starts at `at` (line, column) of the file: a hole in
/// markup, whose positions are then the ones the person sees. The third field says whether it
/// is markup's (see [`Parser::markup`]).
///
/// The SCOPE STACK rides along too (ADR 0066), for the reason above: a `for` inside a hole
/// carries the names bound around the string, and a parser that started with none would
/// read every one of them as local to an iteration.
fn parse_expression_at(src: &str, at: (usize, usize, bool), outer: &Parser) -> Result<Hole, HelixError> {
    let tokens = crate::lexer::lex_at(src, at.0, at.1)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        depth: outer.depth,
        fn_sigs: outer.fn_sigs.clone(),
        do_bindings: Vec::new(),
        imports: outer.imports.clone(),
        selected_imports: outer.selected_imports.clone(),
        glob_import: outer.glob_import,
        pending: Vec::new(),
        temps: 0,
        pinned_keys_at: None,
        markup: at.2,
        scopes: outer.scopes.clone(),
        loops: outer.loops,
        loop_depth: outer.loop_depth,
        rebound: outer.rebound.clone(),
        gone_uses: Vec::new(),
    };
    let loops_before = p.loops;
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
    let loops = p.loops - loops_before;
    Ok((e, p.do_bindings, loops, p.gone_uses))
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
            "for" | "foreach" => return FOR_SHAPE,
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
        return "parentheses make a tuple; a pattern is written in brackets — `[a, b] = p` — in a \
                `let`, a `do` block, a `where` clause or as a statement (and a lambda parameter \
                destructures as `xs.map((a, b) => a + b)`).";
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
    // `total += x` where a statement cannot be: after an expression, on a field or an index.
    if matches!(found, Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq) {
        return "`name += value` is a statement of its own, and only a NAME can be updated: it is \
                `name = name + value`, on a line of a `do` block or a `for` body, or at the top \
                level for a `mut` name. Values do not change in place — a field or an element is \
                rebuilt (`r = {...r, n: r.n + 1}`).";
    }
    "each statement goes on its own line; Helix has no `;`."
}

/// How a loop is written, for every spelling of one that is not Helix's (ADR 0066).
pub(crate) const FOR_SHAPE: &str = "a loop is written `for x in xs { … }` — no colon, no parentheses, the body in braces. \
     `for i in range(n) { … }` counts from 0, `for (k, v) in d.items() { … }` walks a dict, and a \
     name bound before the loop keeps what the body assigns to it (`total += x`).";

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
        anns: Vec::new(),
        defaults: Vec::new(),
        bound: None,
        body: std::rc::Rc::new(Expr::Index {
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
        // The inner node keeps the verb's own name: every engine reads `count_where` as
        // `filter` (a third spelling of that family, as `where` is), so a refusal — a body
        // that is not a yes/no test, a receiver that is not an array — names the verb the
        // user wrote rather than the one it expands to (field build, 1.42).
        _ => m(m(recv, "count_where", vec![f]), "count", vec![]),
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

/// The verbs whose single argument is a function: a bare bound name there is the function
/// it names (`wrap_bound_fn_arg`), for the array reading of every one of them.
pub(crate) const BOUND_FN_VERBS: &[&str] = &[
    "map", "any", "all", "filter", "where", "count_where", "flat_map", "take_while",
    "drop_while", "position", "sort_by", "min_by", "max_by", "map_values",
];

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
/// EVERY verb whose one argument is a function reads it this way — [`BOUND_FN_VERBS`]. It
/// used to be `map`/`any`/`all` alone: `filter`/`where` were held out because
/// `df.where(strong)` is a bare *column* reference and parse time cannot tell the receiver
/// apart, and the verbs the parser desugars were never reached at all — so `filter(P)`
/// refused the predicate `all(P)` accepted (a field build's finding), and `position(f)`,
/// `take_while(f)`, `flat_map(f)`, `min_by(f)` answered `missing`, everything, an array of
/// function values, a comparison error: silent wrong answers a roadmap item had recorded.
/// The origin below is what retires the exemption: the frame reading takes the path
/// (`ast_to_colexpr`), the array reading takes the lambda.
///
/// THE WRAPPER KEEPS ITS ORIGIN (`Lambda.bound`). A record whose field `all` holds a
/// function, called `R.all(U)`, must receive `U` — the parser cannot know the receiver,
/// so it no longer decides: the array reading takes the lambda, and the readings that
/// hand the argument to a function value, or resolve a column name, take the path it
/// came from.
fn wrap_bound_fn_arg(name: &str, args: Vec<Expr>, l: usize, c: usize) -> Vec<Expr> {
    if !BOUND_FN_VERBS.contains(&name) || args.len() != 1 {
        return args;
    }
    let it = || Expr::Ident { name: "it".to_string(), line: l, col: c };
    let body = match &args[0] {
        Expr::Ident { name: f, .. } if f != "it" => {
            Expr::Call { name: f.clone(), args: vec![it()], line: l, col: c }
        }
        Expr::Field { recv, name: f, .. } if is_bound_path(recv) => Expr::Method {
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
    // The origin rides along: a reading that hands this argument to a function VALUE
    // (a record's field, a free fn) passes the path, not the wrapper — see `Lambda.bound`.
    let origin = args.into_iter().next().map(Box::new);
    vec![Expr::Lambda {
        params: vec!["it".to_string()],
        anns: Vec::new(),
        defaults: Vec::new(),
        bound: origin,
        body: std::rc::Rc::new(body),
    }]
}

/// `xs.zipmap(ys, add)` with a bare bound function: the PAIR reaches it as two arguments —
/// `($za, $zb) => add($za, $zb)`, the shape `zipmap`'s own example writes, `(x, y) => x + y`.
/// The `map` the desugar builds took `add` as a body and answered an array of function
/// values, exit 0: the last silent member of the family [`wrap_bound_fn_arg`] reads, and
/// the same rule — a bare name is the function it names. The origin rides along as there.
fn wrap_bound_pair_fn(f: Expr, l: usize, c: usize) -> Expr {
    let binder = |n: &str| Expr::Ident { name: n.to_string(), line: l, col: c };
    let body = match &f {
        Expr::Ident { name, .. } if name != "it" => Expr::Call {
            name: name.clone(),
            args: vec![binder("$za"), binder("$zb")],
            line: l,
            col: c,
        },
        Expr::Field { recv, name, .. } if is_bound_path(recv) => Expr::Method {
            recv: recv.clone(),
            name: name.clone(),
            args: vec![binder("$za"), binder("$zb")],
            named: vec![],
            ufcs: None,
            line: l,
            col: c,
        },
        _ => return f,
    };
    Expr::Lambda {
        params: vec!["$za".to_string(), "$zb".to_string()],
        anns: Vec::new(),
        defaults: Vec::new(),
        bound: Some(Box::new(f)),
        body: std::rc::Rc::new(body),
    }
}

/// A dotted path of NAMES not rooted at `it` — `util.double`, `String.upper` — the one
/// shape [`wrap_bound_fn_arg`] may read as a bound function. A path rooted at `it`
/// (`it.a.b`) is a projection out of each element; and a receiver that is a call, an
/// index, or any other expression makes `.f` the projection it says: `xs.map(mk(0, it).sql)`
/// stays `mk(0, it).sql` rather than becoming `mk(0, it).sql(it)`, which every engine and
/// the checker refused as "`sql` is a field of this record, not a method".
fn is_bound_path(e: &Expr) -> bool {
    match e {
        Expr::Ident { name, .. } => name != "it",
        Expr::Field { recv, .. } => is_bound_path(recv),
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
                Expr::Lambda { params, body, .. } if !params.is_empty() => (params, std::rc::Rc::unwrap_or_clone(body)),
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
            args: vec![Expr::Lambda { params, anns: Vec::new(), defaults: Vec::new(), bound: None, body: std::rc::Rc::new(key) }],
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
        anns: Vec::new(),
        defaults: Vec::new(),
        bound: None,
        body: std::rc::Rc::new(Expr::If {
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
                            anns: Vec::new(),
                            // `x != x` rather than `is_nan(x)`: this desugar is
                            // generated for EVERY receiver type, and `is_nan` on a
                            // String is a static type error — so spelling it that way
                            // broke `["a", "b"].min_by(it)` at check time even though
                            // the branch can never run for strings. `!=` is defined on
                            // every type and is true only for a NaN, which is exactly
                            // the test the guard one level up already uses.
                            defaults: Vec::new(),
                            bound: None,
                            body: std::rc::Rc::new(Expr::Binary {
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
    "Int", "Float", "Num", "String", "Bool", "Array", "Record", "Dict", "Tuple", "Function",
    "Tensor", "DataFrame", "Dna", "Any",
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
    parse_after(tokens, &[])
}

/// [`parse`] for a line of a session that already has bindings — the REPL — whose names are
/// `bound`. A `for` loop and a compound assignment ask whether a name is bound where they
/// stand (ADR 0066), and in a session the earlier lines are where.
pub fn parse_after(tokens: Vec<Token>, bound: &[String]) -> Result<Vec<Stmt>, HelixError> {
    let rebound = rebound_names(&tokens);
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        depth: 0,
        fn_sigs: HashMap::new(),
        do_bindings: Vec::new(),
        imports: std::collections::HashSet::new(),
        selected_imports: std::collections::HashSet::new(),
        glob_import: false,
        pending: Vec::new(),
        temps: 0,
        pinned_keys_at: None,
        markup: false,
        scopes: vec![Scope { names: bound.iter().cloned().collect(), gone: HashMap::new() }],
        loops: 0,
        loop_depth: 0,
        rebound,
        gone_uses: Vec::new(),
    };
    // EVERY SIGNATURE BEFORE THE FIRST CALL IS PARSED — a function may be called above
    // its definition, and its defaults and parameter names must be known there too.
    p.prescan_signatures();
    let mut program = p.program()?;
    // A column expression outside a frame verb's argument is a value — the record that
    // describes it (ADR 0052) — the last pass over the finished tree.
    crate::predicate::desugar_program(&mut program)?;
    reject_do_binding_over_mut_global(&program, &p.do_bindings)?;
    // What the checker may need to explain an unknown name (ADR 0066), now that the
    // file has parsed and it is the checker's turn.
    GONE_USES.with(|t| t.borrow_mut().extend(p.gone_uses.into_iter().map(|(n, l, c, why)| ((n, l, c), why))));
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
        if let Stmt::Assign { name, mutable: true, .. } = s {
            muts.insert(name.as_str());
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
/// The props markup's attribute names map to directly; any other attribute is an HTML
/// attribute (`attrs`), and `aria-*` is `aria`. A `-` in a name is `_` (`drag-axis`).
const MARKUP_PROPS: [&str; 31] = [
    "class", "id", "role", "key", "click", "value", "input", "change", "submit", "dismiss", "scroll", "enter", "exit",
    "layout", "share", "dblclick", "contextmenu", "keydown", "shortcuts", "reveal", "copy", "drag", "drag_axis",
    "drag_commit", "reorder", "resize", "handle", "reorder_group", "drop", "style", "transition",
];

/// HTML's own attribute names (lower case), so a name that is one of them is never mistaken
/// for a typo of one of Helix's props, and a React spelling of one (`tabIndex`) is named.
const HTML_ATTRS: &[&str] = &[
    "accept", "accesskey", "action", "align", "allow", "alt", "async", "autocapitalize", "autocomplete", "autofocus", "autoplay",
    "capture", "charset", "checked", "cite", "cols", "colspan", "content", "contenteditable", "controls", "coords", "crossorigin",
    "datetime", "decoding", "default", "defer", "dir", "dirname", "disabled", "download", "draggable", "enctype", "enterkeyhint",
    "fetchpriority", "for", "form", "formaction", "headers", "height", "hidden", "high", "href", "hreflang", "inert", "inputmode",
    "ismap", "itemprop", "kind", "label", "lang", "list", "loading", "loop", "low", "max", "maxlength", "media", "method", "min",
    "minlength", "multiple", "muted", "name", "nonce", "novalidate", "open", "optimum", "part", "pattern", "ping", "placeholder",
    "playsinline", "popover", "popovertarget", "popovertargetaction", "poster", "preload", "readonly", "referrerpolicy", "rel",
    "required", "reversed", "rows", "rowspan", "sandbox", "scope", "selected", "shape", "size", "sizes", "slot", "span",
    "spellcheck", "src", "srcdoc", "srclang", "srcset", "start", "step", "tabindex", "target", "title", "translate", "type",
    "usemap", "width", "wrap", "xmlns",
];

/// Events a browser has that a view might name, and Helix does not send (so an attribute of
/// that name would never fire).
const DOM_EVENTS: &[&str] = &[
    "blur", "focus", "focusin", "focusout", "doubleclick", "mouseover", "mouseout", "mouseenter", "mouseleave", "mousedown",
    "mouseup", "mousemove", "hover", "keyup", "keypress", "pointerdown", "pointerup", "pointermove", "pointerenter",
    "pointerleave", "touchstart", "touchend", "touchmove", "wheel", "dragstart", "dragend", "dragover", "dragenter",
    "dragleave", "load", "unload", "animationend", "transitionend", "select", "paste", "cut", "beforeinput",
];

/// What to write instead of `on<event>` (or a bare event name Helix does not have).
fn event_hint(ev: &str) -> String {
    let send = |prop: &str, what: &str| format!("a view sends a message instead: `{prop}=\"save\"` posts {{msg: \"save\"}} to `update` {what}.");
    match ev {
        "click" => send("click", "on a click (with `value={…}` as its payload)"),
        "doubleclick" | "dblclick" => send("dblclick", "on a double click"),
        "contextmenu" => send("contextmenu", "on a right click, with where it was"),
        "change" => "for a text field, React's `onChange` is Helix's `input=\"…\"` (every keystroke, with the text as `value`); \
             `change=\"…\"` is sent when a field is committed (a checkbox, a select)."
            .to_string(),
        "input" => send("input", "on every keystroke, with the text as `value`"),
        "submit" => send("submit", "when the form is submitted, with its named fields as `value`"),
        "scroll" => send("scroll", "as the node scrolls"),
        "keydown" | "keyup" | "keypress" => "keys are `keydown=\"key\" shortcuts=\"Enter Escape Ctrl+K ArrowUp\"`: the chords listed are sent \
             as `value` (\"Ctrl+K\", \"Space\", \"ArrowDown\"); there is no key-up event."
            .to_string(),
        "blur" | "focusout" | "focus" | "focusin" => "Helix sends no focus events. To close or cancel something when the person clicks away or \
             presses Escape, use `dismiss=\"msg\"`; to put focus somewhere, use `autofocus` on the element (re-key it to move focus again)."
            .to_string(),
        "mouseover" | "mouseout" | "mouseenter" | "mouseleave" | "hover" | "mousemove" | "pointerenter" | "pointerleave" | "pointermove" => {
            "Helix sends no hover events: a hover look is a utility (`hover:bg-hover`, `group-hover:opacity-100`), and a tooltip is `ui.tooltip`.".to_string()
        }
        "mousedown" | "mouseup" | "pointerdown" | "pointerup" | "touchstart" | "touchend" | "touchmove" | "dragstart" | "dragend"
        | "dragover" | "dragenter" | "dragleave" | "drag" | "drop" => {
            "gestures are props: `drag=\"moved\"` (sent when let go, with `drag: {dx, dy, vx, vy}`), `reorder=\"msg\"` on a list, `drop=\"msg\"` on a target."
                .to_string()
        }
        _ => "Helix's events are `click`, `dblclick`, `contextmenu`, `input`, `change`, `submit`, `dismiss`, `scroll` and \
              `keydown` (with `shortcuts`), plus the gestures `drag`, `reorder`, `resize` and `drop`: `helix describe events`."
            .to_string(),
    }
}

/// Elements whose `value` attribute is HTML's, not a click's payload.
const MARKUP_VALUE_TAGS: [&str; 5] = ["input", "option", "progress", "meter", "data"];

/// `TodoItem` → `todo_item`: a component tag is written as React writes it, and names the
/// Helix function as Helix writes it.
/// Whether a hole's source has a `:` outside every bracket and string — a format spec
/// (`pct:.1f`), since a record's or a call's colons are inside brackets.
fn top_level_colon(src: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None::<char>);
    let mut prev = ' ';
    for c in src.chars() {
        match quote {
            Some(q) => {
                if c == q && prev != '\\' {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ':' if depth == 0 => return true,
                _ => {}
            },
        }
        prev = c;
    }
    false
}

/// `todo_item` → `TodoItem`: how a function is written as a tag.
fn pascal_case(name: &str) -> String {
    name.split('_').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    }).collect()
}

pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

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
    /// Whether this file has a glob import (`import m.*`) — for the same diagnostic: a
    /// named-argument call on a name that is neither local nor a builtin may come from it,
    /// and this parser cannot know which names the glob brings.
    glob_import: bool,
    /// Statements one source statement expanded into, appended by `program` after it:
    /// `{a, b} = r` at the top level is an assignment of the value to a throwaway name and
    /// one assignment per field (`destructure_record_stmt`).
    pending: Vec<Stmt>,
    /// Counter for those throwaway names, so every statement-level destructure binds its
    /// own IMMUTABLE temp — immutable because a `mut` global reads as `Unknown` in the
    /// checker, and the field reads would lose the record's shape with it.
    temps: usize,
    /// The token index of a `{` that may use `@name` keys: the record argument that
    /// opens directly after `.with(` or `.agg(` (ADR 0054). There a key is a column NAME,
    /// and `{@name: e}` pins the column literally called `name` whatever is in scope —
    /// the one escape the defining position lacked. Anywhere else an `@` key is refused
    /// at parse time, so it can only ever reach a frame verb or a runtime refusal.
    pinned_keys_at: Option<usize>,
    /// Whether this parser reads a hole inside markup (ADR 0061), where `if c then <X />` with no
    /// `else` is `missing` — nothing rendered — as `{c && <X />}` is in JSX. Anywhere else an
    /// `if` still needs its `else`.
    markup: bool,
    /// The names bound so far in each enclosing scope, outermost first: the top level, then
    /// every function, lambda, `let`, `do` block, match arm and loop body the cursor is
    /// inside (ADR 0066). Read for one decision — whether a name a `for` body assigns was
    /// bound before the loop, and so is CARRIED from one iteration to the next — and for
    /// the refusal of `n += 1` on a name that is not bound.
    scopes: Vec<Scope>,
    /// How many `for` loops have been read: the number in the next loop's temporaries, and
    /// how a function with a `where` clause knows its body held one.
    loops: usize,
    /// How many `for` bodies the cursor is inside, whatever else is in between: where
    /// `break` and `continue` are answered.
    loop_depth: usize,
    /// Which of the builtins a loop's fold is written with — `type_of`, `raise`, `range` —
    /// this file may have bound to something else ([`rebound_names`]).
    rebound: std::collections::HashSet<&'static str>,
    /// Every read of a name that only a finished loop had bound, for the checker's hint
    /// ([`loop_local_hint`]): handed over when the file has parsed.
    gone_uses: Vec<GoneUse>,
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
        let is_where = matches!(self.peek_at(k), Tok::Ident(n) if n == "where");
        // `where NAME =`, `where {a, b} =` — the record binder ADR 0046 gave `let` — or
        // `where [a, b] =`, the positional one.
        let gate = is_where
            && ((matches!(self.peek_at(k + 1), Tok::Ident(_))
                && matches!(self.peek_at(k + 2), Tok::Eq))
                || (matches!(self.peek_at(k + 1), Tok::LBrace)
                    && self.record_binder_from(self.pos + k + 1))
                || (matches!(self.peek_at(k + 1), Tok::LBracket)
                    && self.array_binder_from(self.pos + k + 1)));
        if !gate {
            // NEAR-MISS teaching (the sweep's three natural mistakes all fell
            // through to a generic "no `;`" error that misdescribed them):
            // `where` followed by a NAME but no `=` is a malformed clause —
            // a missing `=`, an attempted destructure, or a stray token —
            // never two statements.
            if is_where && matches!(self.peek_at(k + 1), Tok::Ident(_) | Tok::LBrace | Tok::LBracket) {
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
                    "a binding is `where NAME = value`, `where {a, b} = record` or `where [a, b] = \
                     pair`; several separate with commas (`where a = 1, b = a + 2`). One `where` per fn.",
                ));
            }
            return Ok(body);
        }
        for _ in 0..=k {
            self.advance(); // the newline run and `where` itself
        }
        let mut bindings: Vec<(String, Expr)> = Vec::new();
        loop {
            if matches!(self.peek(), Tok::LBrace) {
                let from = bindings.len();
                let tmp = format!("$rec{}", bindings.len());
                self.destructure_record(&mut bindings, tmp)?;
                self.bind_all(&bindings[from..]);
            } else if matches!(self.peek(), Tok::LBracket) {
                let from = bindings.len();
                let tmp = format!("$arr{}", bindings.len());
                self.destructure_positions(&mut bindings, tmp)?;
                self.bind_all(&bindings[from..]);
            } else {
                let name = self.ident_name("after `where`")?;
                self.eat(&Tok::Eq, "after the `where` binding's name")
                    .map_err(|e| e.hint("a `where` binding looks like `where LOOKUP = {…}`."))?;
                let value = self.expr()?;
                self.bind(&name);
                bindings.push((name, value));
            }
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
                if !((matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Eq))
                    || matches!(self.peek(), Tok::LBrace))
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
            && matches!(self.peek_at(k2 + 1), Tok::Ident(_) | Tok::LBrace)
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
            let from = stmts.len();
            stmts.push(s);
            stmts.append(&mut self.pending);
            // What the statement bound is bound for everything below it (ADR 0066).
            for s in &stmts[from..] {
                match s {
                    Stmt::Assign { name, .. } => self.bind(name),
                    Stmt::Import { names: ImportNames::Selected(list), .. } => {
                        for n in list {
                            self.bind(n);
                        }
                    }
                    _ => {}
                }
            }
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
            let mut names = ImportNames::Module;
            // Dotted path: `import lib.stats` → `lib/stats.helix`. A `.{a, b}` tail
            // instead selects names to bring into scope unqualified; a `.*` tail brings
            // EVERY export in, minus an optional `except {a, b}` (field build, 1.27.3: a
            // library chains only through an unqualified import, so every verb a caller
            // might chain had to be named up front, and the list grew with the library).
            while matches!(self.peek(), Tok::Dot) {
                self.advance();
                if matches!(self.peek(), Tok::LBrace) {
                    self.advance(); // consume `{`
                    names = ImportNames::Selected(self.import_name_list(
                        "to import inside `{ }`",
                        "a selective import looks like `import lib.stats.{mean, std}`.",
                    )?);
                    break; // nothing follows a selective list
                }
                if matches!(self.peek(), Tok::Star) {
                    self.advance(); // consume `*`
                    // `except` is contextual, exactly like `as`: special only right here.
                    let except = if matches!(self.peek(), Tok::Ident(n) if n == "except") {
                        self.advance();
                        let glob_hint =
                            "decline names from a glob import with `import lib.stats.* except {mean, std}`.";
                        self.eat(&Tok::LBrace, "after `except`").map_err(|e| e.hint(glob_hint))?;
                        self.import_name_list("to decline inside `except { }`", glob_hint)?
                    } else {
                        Vec::new()
                    };
                    names = ImportNames::Glob { except };
                    break; // nothing follows a glob
                }
                segments.push(self.ident_name("after `.` in the module path")?);
            }
            // Optional `as alias` — `as` is contextual (only special here), so it
            // stays usable as an ordinary identifier everywhere else. A selective or glob
            // import binds its names directly, so it takes no alias — and says so, rather
            // than leaving `as x` to fail as the next statement.
            let alias = if matches!(self.peek(), Tok::Ident(n) if n == "as") {
                if !matches!(names, ImportNames::Module) {
                    let (al, ac) = self.pos();
                    return Err(HelixError::new(
                        "this import binds names directly, so it takes no alias",
                        al,
                        ac,
                    )
                    .hint(format!(
                        "`import {0}.{{…}}` and `import {0}.*` bring names in unqualified; \
                         `import {0} as <name>` keeps them under a name.",
                        segments.join(".")
                    )));
                }
                self.advance();
                self.ident_name("after `as`")
                    .map_err(|e| e.hint("give the module an alias, e.g. `import lib.stats as stats`."))?
            } else {
                // Default the namespace to the last path segment.
                segments.last().unwrap().clone()
            };
            // Record the bound namespace so the comprehension-sugar desugars know a
            // method on it is a qualified module call. Selective and glob imports bind
            // names directly and create no namespace, so they are not recorded as one;
            // what they bind is recorded for the named-argument diagnostic.
            match &names {
                ImportNames::Selected(list) => self.selected_imports.extend(list.iter().cloned()),
                ImportNames::Glob { .. } => self.glob_import = true,
                ImportNames::Module => {
                    self.imports.insert(alias.clone());
                }
            }
            return Ok(Stmt::Import { segments, alias, names, line: l, col: c });
        }
        // Optional leading `export` (ADR 0019) — contextual: it's a keyword only
        // directly before a definition (`export fn …`, `export x = …`, `export a, b = …`,
        // `export mut …`, `export {a, b} = …`), so `export` stays a usable identifier
        // everywhere else.
        let exported = matches!(self.peek(), Tok::Ident(n) if n == "export")
            && match self.peek_at(1) {
                Tok::Fn | Tok::Mut => true,
                Tok::Ident(_) => matches!(self.peek_at(2), Tok::Eq | Tok::Comma),
                Tok::LBrace => self.record_binder_from(self.pos + 1),
                Tok::LBracket => self.array_binder_from(self.pos + 1),
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
            let (params, defaults) = self.parse_params()?;
            // `prescan_signatures` already recorded this signature; recording it again
            // keeps the definition authoritative, and costs one small insert.
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
            self.push_scope(params.iter().map(|(n, _)| n.clone()));
            let body = self.fn_body();
            self.pop_scope();
            let body = body?;
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
        // `for x in xs { … }` — a loop (ADR 0066), which the parser writes as a fold.
        if self.for_ahead() {
            let (l, c) = self.pos();
            let out = self.for_loop(true)?;
            return Ok(self.loop_statements(out, l, c));
        }
        if let Some(e) = self.foreign_for() {
            return Err(e);
        }
        // `n += 1` — `n = n + 1`, for a `mut` name (ADR 0066).
        if self.compound_ahead() {
            let (l, c) = self.pos();
            let (name, value) = self.compound_assignment()?;
            return Ok(Stmt::Assign { name, mutable: false, exported: false, value, line: l, col: c });
        }
        // `mut x = ...` or `mut a, b = ...`
        if matches!(self.peek(), Tok::Mut) {
            let (l, c) = self.pos();
            self.advance();
            if matches!(self.peek(), Tok::LBrace) {
                return self.destructure_record_stmt(true, exported, l, c);
            }
            if matches!(self.peek(), Tok::LBracket) {
                return self.destructure_positions_stmt(true, exported, l, c);
            }
            let first = self.ident_name("after `mut`")?;
            if matches!(self.peek(), Tok::Comma) {
                let (nl, nc) = (l, c);
                let names = self.finish_target_list(first)?;
                self.eat(&Tok::Eq, "in destructuring assignment")?;
                let value = self.expr()?;
                return self.positional_stmt(names, value, true, exported, nl, nc);
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
        // `{a, b} = …` — record destructuring as a statement (ADR 0046 addendum).
        if matches!(self.peek(), Tok::LBrace) && self.record_binder_ahead() {
            let (l, c) = self.pos();
            return self.destructure_record_stmt(false, exported, l, c);
        }
        // `[a, b] = …` — positional destructuring as a statement (ADR 0053).
        if matches!(self.peek(), Tok::LBracket) && self.array_binder_ahead() {
            let (l, c) = self.pos();
            return self.destructure_positions_stmt(false, exported, l, c);
        }
        // `a, b = …` — the same, in the bare spelling a statement has always taken.
        if self.at_destructure() {
            let (l, c) = self.pos();
            let first = self.ident_name("")?;
            let names = self.finish_target_list(first)?;
            self.eat(&Tok::Eq, "in destructuring assignment")?;
            let value = self.expr()?;
            return self.positional_stmt(names, value, false, exported, l, c);
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

    /// The `{a, b,}` body of a selective import or of an `except` list, after its `{`: one
    /// or more names, an optional trailing comma, the closing brace.
    fn import_name_list(&mut self, ctx: &str, hint: &str) -> Result<Vec<String>, HelixError> {
        let mut names = Vec::new();
        loop {
            names.push(self.ident_name(ctx)?);
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                if matches!(self.peek(), Tok::RBrace) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        self.eat(&Tok::RBrace, "to close the import list").map_err(|e| e.hint(hint))?;
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
                let e = HelixError::new(format!("expected a name {}, found {}", ctx, other.describe()), l, c);
                // A keyword where a name goes: say it is one, and which they are.
                let kw = matches!(
                    other,
                    Tok::Mut | Tok::Fn | Tok::Import | Tok::And | Tok::Or | Tok::Not | Tok::If | Tok::Then | Tok::Else
                        | Tok::Let | Tok::In | Tok::Missing | Tok::Try | Tok::Match | Tok::Do | Tok::True | Tok::False
                );
                Err(if kw {
                    e.hint(format!(
                        "{} is a reserved word, so it cannot be a name — pick another (`then_at`, `when`). The reserved \
                         words are: and, do, else, false, fn, if, import, in, let, match, missing, mut, not, or, then, true, try.",
                        other.describe()
                    ))
                } else {
                    e
                })
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

    /// A parameter list, `(` to `)`: names, optional `: Type` annotations, optional
    /// literal defaults. One parser for the definition AND for `prescan_signatures`, so a
    /// signature reads the same from both.
    fn parse_params(&mut self) -> Result<Params, HelixError> {
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
                        .hint("defaults like `= 0`, `= -5`, `= \"\"`, `= true`, `= missing`, `= []` or `= {}` are allowed — literals, and literal containers of literals."));
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
        Ok((params, defaults))
    }

    /// Record the signature of every `fn NAME(…)` in the file before any of it is parsed,
    /// with `parse_params` — the definition's own parameter parser — so a call written
    /// ABOVE its definition fills omitted arguments from the defaults and places named
    /// arguments exactly as one written below it does. Until this scan, `fn_sigs` was
    /// filled as each definition was reached, and a forward call found nothing: its
    /// defaults were never filled, so the checker and both engines refused it for arity,
    /// and its named arguments were refused as "only supported for user-defined
    /// functions" — about a function three lines down. The same program with the
    /// definition moved above the call was fine.
    ///
    /// One pass is enough: a default is a literal constant (`is_const_default`), never a
    /// call that would need another signature first. A parameter list that does not
    /// parse is skipped here, silently, because the definition's own parse reports it —
    /// at the right place, in the right words. The cursor and depth are restored after.
    fn prescan_signatures(&mut self) {
        let saved = (self.pos, self.depth);
        for i in 0..self.toks.len().saturating_sub(2) {
            if !matches!(self.toks[i].tok, Tok::Fn) || !matches!(self.toks[i + 2].tok, Tok::LParen) {
                continue;
            }
            let Tok::Ident(name) = &self.toks[i + 1].tok else { continue };
            let name = name.clone();
            self.pos = i + 2;
            self.depth = 0;
            if let Ok((params, defaults)) = self.parse_params() {
                self.fn_sigs.insert(
                    name,
                    FnSig { params: params.into_iter().map(|(n, _)| n).collect(), defaults },
                );
            }
        }
        (self.pos, self.depth) = saved;
    }

    /// `{a, b} = value` — one binding per named field. The value is bound ONCE to a
    /// throwaway name (`$rec<N>`; `$` cannot appear in user code, so it shadows nothing)
    /// and each field is read from it through `FieldOrMissing`: the value, or `missing`
    /// for a field the record does not have — the answer `get` gives, and what makes a
    /// destructure fit a spec record whose keys are optional by nature. The reads are
    /// field reads, not `get` calls: one symbol scan per name, no method dispatch.
    fn destructure_record(
        &mut self,
        bindings: &mut Vec<(String, Expr)>,
        tmp: String,
    ) -> Result<(), HelixError> {
        let shape = "destructuring looks like `let {where, limit: lim} = spec in …`.";
        self.eat(&Tok::LBrace, "to start the fields to destructure")?;
        // (field, binder, line, col) — the binder is the field's own name, or the name
        // after a colon: `{select: sel}` reads the field `select` and binds `sel`, so a
        // module that DEFINES `select` can still destructure a spec that names it (field
        // build, 1.43; ADR 0046's addendum).
        let mut names: Vec<(String, String, usize, usize)> = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace) && !names.is_empty() {
                break; // a trailing comma
            }
            let (nl, nc) = self.pos();
            let name = self.ident_name("as a field to destructure")?;
            let binder = if matches!(self.peek(), Tok::Colon) {
                self.advance();
                self.ident_name("to bind the field under")
                    .map_err(|e| e.hint(format!("`{{{name}: other}}` reads the field `{name}` and binds `other`.")))?
            } else {
                name.clone()
            };
            names.push((name, binder, nl, nc));
            self.skip_newlines();
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.skip_newlines();
        self.eat(&Tok::RBrace, "to close the fields to destructure").map_err(|e| e.hint(shape))?;
        self.eat(&Tok::Eq, "after the fields to destructure").map_err(|e| e.hint(shape))?;
        let value = self.expr()?;
        bindings.push((tmp.clone(), value));
        for (name, binder, nl, nc) in names {
            let recv = Box::new(Expr::Ident { name: tmp.clone(), line: nl, col: nc });
            bindings.push((binder, Expr::FieldOrMissing { recv, name, line: nl, col: nc }));
        }
        Ok(())
    }

    /// `{a, b} = value` as a STATEMENT: the `let` desugar spread over assignments —
    /// `$rec<N> = value` (immutable, its own name), then `a = $rec<N>.a`, `b = …` through
    /// `FieldOrMissing` — the first returned, the rest queued in `pending` for `program` to
    /// append. Every rule an assignment has — `mut`, `export`, rebinding an immutable, the
    /// module's export list, the checker's shape refusal — applies to each field binding
    /// unchanged, on every engine, because each IS an assignment. The temp is never
    /// exported and never `mut`.
    fn destructure_record_stmt(
        &mut self,
        mutable: bool,
        exported: bool,
        l: usize,
        c: usize,
    ) -> Result<Stmt, HelixError> {
        let tmp = format!("$rec{}", self.temps);
        self.temps += 1;
        let mut bindings = Vec::new();
        self.destructure_record(&mut bindings, tmp)?;
        self.assignments_of(bindings, true, mutable, exported, l, c)
    }

    /// `[a, b] = value`, `[a, b, ...rest] = value` — one binding per position (ADR 0053).
    /// The value is bound ONCE to a throwaway name (`$arr<N>`; `$` cannot appear in user
    /// code, so it shadows nothing) and each position is read from it through `Part`, which
    /// holds the value to the pattern: a tuple or an array of exactly as many parts as there
    /// are names — or at least as many, when the pattern ends in a rest, which is then bound
    /// to what is left. Nothing downstream knows the form exists.
    fn destructure_positions(
        &mut self,
        bindings: &mut Vec<(String, Expr)>,
        tmp: String,
    ) -> Result<bool, HelixError> {
        let shape = "destructuring looks like `let [a, b] = pair in …`, or `[first, ...rest] = xs`.";
        let (l, c) = self.pos();
        self.eat(&Tok::LBracket, "to start the positions to destructure")?;
        let mut names: Vec<(String, usize, usize)> = Vec::new();
        let mut rest: Option<(String, usize, usize)> = None;
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBracket) && (!names.is_empty() || rest.is_some()) {
                break; // a trailing comma
            }
            let (nl, nc) = self.pos();
            if matches!(self.peek(), Tok::DotDotDot) {
                if rest.is_some() {
                    return Err(HelixError::new("a pattern takes one `...rest`", nl, nc).hint(shape));
                }
                self.advance();
                let name = self.ident_name("after `...` — the name for the rest")?;
                rest = Some((name, nl, nc));
                self.skip_newlines();
                if matches!(self.peek(), Tok::Comma) {
                    self.advance();
                    self.skip_newlines();
                }
                if !matches!(self.peek(), Tok::RBracket) {
                    let (el, ec) = self.pos();
                    return Err(HelixError::new("`...rest` must come last in a pattern", el, ec).hint(shape));
                }
                break;
            }
            let name = self.ident_name("as a position to destructure")?;
            names.push((name, nl, nc));
            self.skip_newlines();
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.skip_newlines();
        self.eat(&Tok::RBracket, "to close the positions to destructure").map_err(|e| e.hint(shape))?;
        if names.is_empty() && rest.is_none() {
            return Err(HelixError::new("a pattern needs at least one name", l, c).hint(shape));
        }
        Self::refuse_twice(names.iter().chain(&rest))?;
        self.eat(&Tok::Eq, "after the positions to destructure").map_err(|e| e.hint(shape))?;
        let value = self.expr()?;
        Ok(Self::push_positions(bindings, tmp, names, rest, value))
    }

    /// A name twice in one pattern binds nothing twice: refused by name, where the two
    /// assignments it would desugar to would only say the second reassigns the first.
    fn refuse_twice<'a>(names: impl Iterator<Item = &'a (String, usize, usize)>) -> Result<(), HelixError> {
        let mut seen: Vec<&str> = Vec::new();
        for (name, l, c) in names {
            if seen.contains(&name.as_str()) {
                return Err(HelixError::new(format!("`{name}` appears twice in this pattern"), *l, *c)
                    .hint("each position binds its own name."));
            }
            seen.push(name);
        }
        Ok(())
    }

    /// The desugar itself: the value under `tmp`, then one `Part` read per name — and one
    /// for the rest, at the index after the last name. A value that is already a NAME needs
    /// no temp — `[a, b] = p` reads `p` twice, pure and one lookup each, and the binding it
    /// saves is a third of the form's cost — unless a pattern name is `p` itself, where the
    /// first read would replace what the second reads from. Answers whether a temp was bound.
    fn push_positions(
        bindings: &mut Vec<(String, Expr)>,
        tmp: String,
        names: Vec<(String, usize, usize)>,
        rest: Option<(String, usize, usize)>,
        value: Expr,
    ) -> bool {
        let count = names.len();
        let has_rest = rest.is_some();
        let source = match &value {
            Expr::Ident { name, .. } if !names.iter().chain(&rest).any(|(n, _, _)| n == name) => name.clone(),
            _ => {
                bindings.push((tmp.clone(), value));
                tmp
            }
        };
        let has_temp = source.starts_with('$');
        for (index, (name, nl, nc)) in names.into_iter().chain(rest).enumerate() {
            let recv = Box::new(Expr::Ident { name: source.clone(), line: nl, col: nc });
            bindings.push((name, Expr::Part { recv, index, names: count, rest: has_rest, line: nl, col: nc }));
        }
        has_temp
    }

    /// `[a, b] = value` as a STATEMENT — and `a, b = value`, its bare spelling, through
    /// `positional_stmt`: the `let` desugar spread over assignments, exactly as the record
    /// form is (`destructure_record_stmt`). The first assignment is returned, the rest
    /// queued in `pending`; the temp is never exported and never `mut`.
    fn destructure_positions_stmt(
        &mut self,
        mutable: bool,
        exported: bool,
        l: usize,
        c: usize,
    ) -> Result<Stmt, HelixError> {
        let tmp = format!("$arr{}", self.temps);
        self.temps += 1;
        let mut bindings = Vec::new();
        let has_temp = self.destructure_positions(&mut bindings, tmp)?;
        self.assignments_of(bindings, has_temp, mutable, exported, l, c)
    }

    /// The bare statement `a, b = value`: the same desugar as `[a, b] = value` (a bare
    /// pattern has no rest — that spelling needs the brackets).
    fn positional_stmt(
        &mut self,
        names: Vec<String>,
        value: Expr,
        mutable: bool,
        exported: bool,
        l: usize,
        c: usize,
    ) -> Result<Stmt, HelixError> {
        let tmp = format!("$arr{}", self.temps);
        self.temps += 1;
        let mut bindings = Vec::new();
        let names: Vec<(String, usize, usize)> = names.into_iter().map(|n| (n, l, c)).collect();
        Self::refuse_twice(names.iter())?;
        let has_temp = Self::push_positions(&mut bindings, tmp, names, None, value);
        self.assignments_of(bindings, has_temp, mutable, exported, l, c)
    }

    /// A destructure's bindings as top-level assignments: the first returned, the rest
    /// queued; each name `mut` and `export` as written, and the temp — when there is one, it
    /// comes first — neither.
    fn assignments_of(
        &mut self,
        bindings: Vec<(String, Expr)>,
        has_temp: bool,
        mutable: bool,
        exported: bool,
        l: usize,
        c: usize,
    ) -> Result<Stmt, HelixError> {
        let mut it = bindings.into_iter();
        // A pattern binds at least one name; the `ok_or_else` is a refusal rather than a
        // panic on purpose.
        let (name, value) =
            it.next().ok_or_else(|| HelixError::new("destructuring binds nothing", l, c))?;
        let (m, x) = if has_temp { (false, false) } else { (mutable, exported) };
        let first = Stmt::Assign { name, mutable: m, exported: x, value, line: l, col: c };
        self.pending.extend(
            it.map(|(name, value)| Stmt::Assign { name, mutable, exported, value, line: l, col: c }),
        );
        Ok(first)
    }

    /// Whether the `[` at the cursor opens a positional binder — `[a, b] = …`,
    /// `[a, ...rest] = …` — rather than an array expression: names and commas (a `...name`
    /// last) up to `]`, then a single `=`. `==` is its own token, so nothing that is an
    /// expression matches. Decided by looking, like `record_binder_ahead`.
    fn array_binder_ahead(&self) -> bool {
        self.array_binder_from(self.pos)
    }

    /// Deliberately loose about the INSIDE — any run of names, commas and `...` — so that a
    /// pattern with its rest in the wrong place, two rests, or no names at all reaches
    /// `destructure_positions` and is refused in its own words, rather than parsing as an
    /// array expression and failing on the `=` after it.
    fn array_binder_from(&self, at: usize) -> bool {
        let tok = |i: usize| self.toks.get(i).map(|t| &t.tok);
        let mut i = at + 1;
        loop {
            match tok(i) {
                Some(Tok::Ident(_) | Tok::Comma | Tok::DotDotDot | Tok::Newline) => i += 1,
                Some(Tok::RBracket) => {
                    i += 1;
                    break;
                }
                _ => return false,
            }
        }
        matches!(tok(i), Some(Tok::Eq))
    }

    /// Whether the `{` at the cursor opens a destructuring binder — `{a, b} = …` — rather
    /// than a record expression: names and commas up to `}`, then a single `=`. A record
    /// literal has a `:` after its first name and `==` is its own token, so nothing that
    /// is an expression matches. Decided by looking, not parsing, so a record that merely
    /// begins a `do` block's result expression is left alone.
    fn record_binder_ahead(&self) -> bool {
        self.record_binder_from(self.pos)
    }

    /// `record_binder_ahead` for a `{` at token index `at` — `export {a, b} = …` asks it
    /// about the token after `export`.
    fn record_binder_from(&self, at: usize) -> bool {
        let tok = |i: usize| self.toks.get(i).map(|t| &t.tok);
        let mut i = at + 1;
        loop {
            while matches!(tok(i), Some(Tok::Newline)) {
                i += 1;
            }
            if !matches!(tok(i), Some(Tok::Ident(_))) {
                return false;
            }
            i += 1;
            while matches!(tok(i), Some(Tok::Newline)) {
                i += 1;
            }
            // `field: binder` — the rename form (ADR 0046 addendum). A record LITERAL
            // `{a: 1}` has a value after the colon, not a name, so it still reads as one;
            // `{a: b}` with a name is a destructure only when `=` follows, which the check
            // below still requires.
            if matches!(tok(i), Some(Tok::Colon)) {
                i += 1;
                while matches!(tok(i), Some(Tok::Newline)) {
                    i += 1;
                }
                if !matches!(tok(i), Some(Tok::Ident(_))) {
                    return false;
                }
                i += 1;
                while matches!(tok(i), Some(Tok::Newline)) {
                    i += 1;
                }
            }
            match tok(i) {
                Some(Tok::Comma) => {
                    i += 1;
                    while matches!(tok(i), Some(Tok::Newline)) {
                        i += 1;
                    }
                    if matches!(tok(i), Some(Tok::RBrace)) {
                        i += 1;
                        break;
                    }
                }
                Some(Tok::RBrace) => {
                    i += 1;
                    break;
                }
                _ => return false,
            }
        }
        matches!(tok(i), Some(Tok::Eq))
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
            "Record" => TypeAnn::Record,
            "Dict" => TypeAnn::Dict,
            "Tuple" => TypeAnn::Tuple,
            "Function" => TypeAnn::Function,
            "Any" => TypeAnn::Any,
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
                self.push_scope([name.clone()]);
                let body = self.expr();
                self.pop_scope();
                let body = body?;
                self.depth = saved;
                return Ok(Some(Expr::Lambda {
                    params: vec![name],
                    anns: Vec::new(),
                    defaults: Vec::new(),
                    bound: None,
                    body: std::rc::Rc::new(body),
                }));
            }
            return Ok(None);
        }

        // zero or more parameters — `() =>`, `(a, b) =>`, `(a, n = 10) =>` — as a LOOK, not
        // a parse: the scan only decides whether `=>` follows a parameter list. The list
        // is then parsed by `parse_params`, the definition's own parser, so a lambda
        // default reads exactly as a `fn` default does: a literal constant, trailing.
        if matches!(self.peek(), Tok::LParen) {
            let mut k = self.pos + 1;
            // A zero-arg lambda `() => body` — a thunk (e.g. for benchmark harnesses).
            // Only commit if `=>` follows, so a bare `()` stays an ordinary expression.
            if matches!(self.toks[k].tok, Tok::RParen) {
                k += 1;
            } else {
                loop {
                    if !matches!(self.toks[k].tok, Tok::Ident(_)) {
                        return Ok(None); // non-ident in param list — not a lambda
                    }
                    k += 1;
                    // `x: Int` — an annotated parameter (field build, 1.45b). The type name is
                    // one identifier, exactly what `parse_params` reads afterwards; anything
                    // else after the colon is not a lambda's parameter list.
                    if matches!(self.toks[k].tok, Tok::Colon) {
                        k += 1;
                        if !matches!(self.toks[k].tok, Tok::Ident(_)) {
                            return Ok(None);
                        }
                        k += 1;
                    }
                    if matches!(self.toks[k].tok, Tok::Eq) {
                        // `= …`: skip the default's tokens to the `,` or `)` that ends it,
                        // bracket-aware. What a default may BE is `parse_params`' decision
                        // — a literal constant — so `(x, n = x) => n` is refused by that
                        // rule, in its words, rather than falling to the tuple parser.
                        k += 1;
                        let mut depth = 0usize;
                        loop {
                            match &self.toks[k].tok {
                                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                                Tok::RParen | Tok::RBracket | Tok::RBrace if depth > 0 => depth -= 1,
                                Tok::RParen => break,
                                // A comma ends the default only at depth 0: `t = (1, 2)`
                                // has one inside its parentheses (a tuple default on a
                                // lambda was refused as an unclosed tuple — field build).
                                Tok::Comma if depth == 0 => break,
                                Tok::Newline | Tok::Eof => return Ok(None),
                                _ => {}
                            }
                            k += 1;
                        }
                    }
                    match &self.toks[k].tok {
                        Tok::Comma => k += 1,
                        Tok::RParen => {
                            k += 1;
                            break;
                        }
                        _ => return Ok(None),
                    }
                }
            }
            if !matches!(self.toks[k].tok, Tok::FatArrow) {
                return Ok(None);
            }
            let (params, defaults) = self.parse_params()?;
            self.eat(&Tok::FatArrow, "after the parameter list")?;
            let saved = self.depth;
            self.deepen()?;
            self.push_scope(params.iter().map(|(n, _)| n.clone()));
            let body = self.expr();
            self.pop_scope();
            let body = body?;
            self.depth = saved;
            // The annotations `parse_params` read — `(x: Int) => x` — ride along; they
            // used to be dropped here, so a lambda's parameters could not be typed.
            let anns: Vec<Option<TypeAnn>> = params.iter().map(|(_, a)| a.clone()).collect();
            return Ok(Some(Expr::Lambda {
                params: params.into_iter().map(|(n, _)| n).collect(),
                anns,
                defaults: defaults.into_iter().flatten().collect(),
                bound: None,
                body: std::rc::Rc::new(body),
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
            // `x == missing` is never true or false: under ADR 0001 a comparison with an
            // unknown is unknown, so it is `missing`, and the `if` (or `where`) that reads it
            // fails far from here, or selects nothing. Said at the comparison instead. Only one
            // side being the literal counts: `missing == missing` is how the rule is shown.
            if matches!(left, Expr::Missing) != matches!(right, Expr::Missing) {
                let other = "x";
                let neg = op == BinOp::Ne;
                self.depth = saved;
                return Err(HelixError::new(
                    format!("`{} missing` is never true or false — it is `missing`", if neg { "!=" } else { "==" }),
                    l,
                    c,
                )
                .hint(format!(
                    "test for it with `{}{other}.is_missing()` (in a query, `{}@col.is_missing()`), or give a default first: `({other} ?? \"\") == \"pro\"`.",
                    if neg { "not " } else { "" },
                    if neg { "not " } else { "" },
                )));
            }
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
                    // WRAPPING, because the operand is not always a non-negative literal:
                    // `- -9223372036854775808` hands this arm the i64::MIN the inner minus
                    // folded below. `-x` wraps at run time (docs/integer-semantics.md), so
                    // the fold must too — a plain `-` agreed in a release build and
                    // aborted a debug one. Pinned by `tests/corpus/neg_min_literal`.
                    Expr::Int(n) => Ok(Expr::Int(n.wrapping_neg())),
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
                        // A record opening exactly here may pin a key with `@` (ADR 0054).
                        if name == "with" || name == "agg" {
                            self.pinned_keys_at = Some(self.pos);
                        }
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
                        // A bare bound function as the one argument, for EVERY verb that
                        // takes one — before the desugars, which build on `args`.
                        let args = wrap_bound_fn_arg(&name, args, l, c);
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
                            // `xs.reduce(0, add)` / `xs.scan(0, f)`: a bare bound name as the
                            // FOLDING function is the function it names — the pair rule
                            // `zipmap` already reads, for the two folds whose function is
                            // the second argument (field build: refused with "name both
                            // binders" while every single-function verb took a bare name).
                            "reduce" | "scan" if args.len() == 2 => {
                                let mut it = args.into_iter();
                                let (init, f) = match (it.next(), it.next()) {
                                    (Some(init), Some(f)) => (init, wrap_bound_pair_fn(f, l, c)),
                                    _ => {
                                        return Err(HelixError::new(
                                            format!("`{name}` takes (init, fn)"),
                                            l,
                                            c,
                                        ))
                                    }
                                };
                                Expr::Method {
                                    recv: Box::new(e),
                                    name: name.clone(),
                                    args: vec![init, f],
                                    named: vec![],
                                    ufcs: None,
                                    line: l,
                                    col: c,
                                }
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
                                let f = wrap_bound_pair_fn(it.next().unwrap(), l, c);
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
                                args,
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
                // A glob-imported function is user-defined too — and this parser cannot
                // even know its NAME, only that a glob exists. A name that is not a
                // builtin can only have come from it (or be undefined, which the loader
                // reports in its own words).
                if self.glob_import && !crate::registry::is_builtin_name(name) {
                    return Err(HelixError::new(
                        format!(
                            "`{name}` may come from a glob import (`import ….*`), so its \
                             parameter names are not visible here"
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
    /// A match arm after its pattern: the optional guard, `=>`, the result.
    fn match_arm_rest(&mut self) -> Result<(Option<Expr>, Expr), HelixError> {
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
        Ok((guard, self.expr()?))
    }

    /// A `let` after its keyword: the bindings, `in`, the body.
    fn let_rest(&mut self) -> Result<Expr, HelixError> {
        let mut bindings = Vec::new();
        loop {
            if matches!(self.peek(), Tok::Mut) {
                let (ml, mc) = self.pos();
                return Err(mut_inside_a_body(ml, mc));
            }
            let from = bindings.len();
            if matches!(self.peek(), Tok::LBrace) {
                let tmp = format!("$rec{}", bindings.len());
                self.destructure_record(&mut bindings, tmp)?;
            } else if matches!(self.peek(), Tok::LBracket) {
                let tmp = format!("$arr{}", bindings.len());
                self.destructure_positions(&mut bindings, tmp)?;
            } else {
                let name = self.ident_name("as a `let` binding")?;
                self.eat(&Tok::Eq, "in a `let` binding")
                    .map_err(|e| e.hint("`let` looks like `let a = 1, b = 2 in a + b`."))?;
                let value = self.expr()?;
                bindings.push((name, value));
            }
            self.bind_all(&bindings[from..]);
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.eat(&Tok::In, "after `let` bindings")
            .map_err(|e| e.hint("a `let` needs an `in` body: `let x = 1 in x + 1`."))?;
        let body = self.expr()?;
        Ok(Expr::Let { bindings, body: Box::new(body), from_do: false })
    }

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
            // `for x in xs { … }` — a loop (ADR 0066): the fold, then one binding per name
            // it carries, each a line of this block like any other.
            if self.for_ahead() {
                let out = self.for_loop(false)?;
                self.bind_all(&out.bindings);
                bindings.extend(out.bindings);
                self.end_of_line("the `for` loop")?;
                continue;
            }
            if let Some(e) = self.foreign_for() {
                return Err(e);
            }
            // `n += 1` — `n = n + 1`, a binding like the one below (ADR 0066).
            if self.compound_ahead() {
                let (bl, bc) = self.pos();
                let (name, value) = self.compound_assignment()?;
                self.do_bindings.push((name.clone(), bl, bc));
                self.bind(&name);
                bindings.push((name, value));
                self.skip_newlines();
                continue;
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
                self.bind(&name);
                bindings.push((name, value));
                self.skip_newlines();
                continue;
            }
            // `{a, b} = value` — the same destructure `let` accepts, as a block binding.
            if matches!(self.peek(), Tok::LBrace) && self.record_binder_ahead() {
                let (bl, bc) = self.pos();
                let first = bindings.len();
                let tmp = format!("$rec{first}");
                self.destructure_record(&mut bindings, tmp)?;
                for (name, _) in &bindings[first + 1..] {
                    self.do_bindings.push((name.clone(), bl, bc));
                }
                self.bind_all(&bindings[first..]);
                self.skip_newlines();
                continue;
            }
            // `[a, b] = value` — the positional one (ADR 0053).
            if matches!(self.peek(), Tok::LBracket) && self.array_binder_ahead() {
                let (bl, bc) = self.pos();
                let first = bindings.len();
                let tmp = format!("$arr{first}");
                let has_temp = self.destructure_positions(&mut bindings, tmp)?;
                for (name, _) in &bindings[first + usize::from(has_temp)..] {
                    self.do_bindings.push((name.clone(), bl, bc));
                }
                self.bind_all(&bindings[first..]);
                self.skip_newlines();
                continue;
            }
            // `a, b = value` inside a block: the bare spelling is a statement's; in a block
            // the pattern is written in brackets. Named, rather than "unexpected `,`".
            if self.at_destructure() {
                let (bl, bc) = self.pos();
                return Err(HelixError::new("a pattern inside a block is written in brackets", bl, bc)
                    .hint("`[a, b] = value` — the bare `a, b = value` is a top-level statement."));
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
            Expr::RecordUpdate { parts, line, col } => {
                *line = l;
                *col = c;
                for p in parts {
                    Self::relocate(p.expr_mut(), l, c);
                }
            }
            Expr::Field { recv, line, col, .. }
            | Expr::FieldOrMissing { recv, line, col, .. }
            | Expr::Part { recv, line, col, .. } => {
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
            Expr::Lambda { defaults, bound, body, .. } => {
                for d in defaults.iter_mut() {
                    Self::relocate(d, l, c);
                }
                if let Some(origin) = bound {
                    Self::relocate(origin, l, c);
                }
                Self::relocate(std::rc::Rc::make_mut(body), l, c);
            }
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

    // --- Markup (ADR 0061). ----------------------------------------------------------------
    //
    // `<li class="row" click="open" value={f.id}>{f.name}</li>` is the record `ui.el` builds,
    // `{tag: "li", props: {class: "row", click: "open", value: f.id}, children: [f.name]}`, and
    // `<ui.Button caption="Save" msg="save"/>` is the call `ui.button(caption: "Save", msg:
    // "save")`. Everything is decided here, so the checker and the engines see only records,
    // arrays and calls. A child that is a string, a number, a list, `missing` or a Bool is
    // made a node (or nothing) by the renderer, which is what lets `{items.map(row)}` and
    // `{if busy then <Spinner/> else missing}` sit among the children.

    fn markup(&mut self, n: &crate::markup::MNode) -> Result<Expr, HelixError> {
        self.deepen()?;
        let r = self.markup_node(n);
        self.depth -= 1;
        r
    }

    fn markup_node(&mut self, n: &crate::markup::MNode) -> Result<Expr, HelixError> {
        use crate::markup::MNode;
        match n {
            MNode::Text(t) => Ok(Expr::Record(vec![
                ("tag".into(), Expr::Str("#text".into())),
                ("props".into(), Expr::Record(vec![])),
                ("children".into(), Expr::Array(vec![])),
                ("text".into(), Expr::Str(t.clone())),
            ])),
            // `{pct:.1f}` among the children formats, as it does in a string: such a hole is the
            // interpolated string `"{pct:.1f}"`, so the spec means exactly what it means there.
            MNode::Hole(src) if top_level_colon(&src.text) => {
                let as_string = crate::markup::Src { text: format!("'''{{{}}}'''", src.text), line: src.line, col: src.col.saturating_sub(4).max(1) };
                self.markup_src(&as_string)
            }
            MNode::Hole(src) => self.markup_src(src),
            MNode::Fragment { children, .. } => Ok(Expr::Array(self.markup_children(children)?)),
            MNode::Element { tag, attrs, children, line, col } => {
                if tag.contains('.') || tag.starts_with(|c: char| c.is_ascii_uppercase()) {
                    self.markup_component(tag, attrs, children, *line, *col)
                } else {
                    self.markup_element(tag, attrs, children, *line, *col)
                }
            }
        }
    }

    fn markup_children(&mut self, kids: &[crate::markup::MNode]) -> Result<Vec<Expr>, HelixError> {
        kids.iter().map(|k| self.markup(k)).collect()
    }

    /// A hole's expression, parsed where it was written.
    fn markup_src(&mut self, s: &crate::markup::Src) -> Result<Expr, HelixError> {
        let (e, binds, loops, gone) = parse_expression_at(&s.text, (s.line, s.col, true), self)?;
        self.do_bindings.extend(binds);
        self.loops += loops;
        self.gone_uses.extend(gone);
        Ok(e)
    }

    fn markup_value(&mut self, a: &crate::markup::MAttr) -> Result<Expr, HelixError> {
        use crate::markup::MValue;
        match &a.value {
            MValue::Flag => Ok(Expr::Bool(true)),
            MValue::Str(s) | MValue::Expr(s) | MValue::Spread(s) => self.markup_src(s),
        }
    }

    fn markup_element(
        &mut self,
        tag: &str,
        attrs: &[crate::markup::MAttr],
        children: &[crate::markup::MNode],
        line: usize,
        col: usize,
    ) -> Result<Expr, HelixError> {
        use crate::ast::RecordPart;
        use crate::markup::MValue;
        let mut parts: Vec<RecordPart> = Vec::new();
        let mut spread = false;
        let mut aria: Vec<(String, Expr)> = Vec::new();
        let mut raw: Vec<(String, Expr)> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        // `class:on={cond}`: the class `on` while `cond` holds, added to `class`.
        let mut toggles: Vec<Expr> = Vec::new();
        let mut class_at: Option<usize> = None;
        for a in attrs {
            if let MValue::Spread(_) = a.value {
                let e = self.markup_value(a)?;
                parts.push(RecordPart::Spread(e));
                spread = true;
                continue;
            }
            let name = a.name.as_str();
            if seen.contains(&name) {
                return Err(HelixError::new(format!("`{name}` is given twice on `<{tag}>`"), a.line, a.col)
                    .hint("give each attribute once; the second would silently win."));
            }
            seen.push(name);
            let lower = name.to_ascii_lowercase();
            if name == "className" || name == "class_name" {
                return Err(HelixError::new("the attribute is `class`", a.line, a.col)
                    .hint("write `class=\"…\"`: Helix markup is HTML's names, so `class` and `for`."));
            }
            if lower.len() > 2 && lower.starts_with("on") && lower[2..].chars().all(|c| c.is_ascii_alphabetic()) && lower != "open" {
                return Err(HelixError::new(format!("`{name}` would be a script handler, which a view cannot carry"), a.line, a.col)
                    .hint(event_hint(&lower[2..])));
            }
            // An event name written without `on` that Helix does not have would be a dead
            // attribute: `blur="save"` checks, renders, and never fires.
            if !MARKUP_PROPS.contains(&lower.as_str()) && DOM_EVENTS.contains(&lower.as_str()) {
                return Err(HelixError::new(format!("Helix has no `{lower}` event"), a.line, a.col).hint(event_hint(&lower)));
            }
            if let Some(class) = name.strip_prefix("class:") {
                if class.is_empty() {
                    return Err(HelixError::new("`class:` needs a class name: `class:active={on}`", a.line, a.col));
                }
                let cond = self.markup_value(a)?;
                toggles.push(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(Expr::Str(class.to_string())),
                    else_branch: Box::new(Expr::Missing),
                    line: a.line,
                    col: a.col,
                });
                continue;
            }
            let value = self.markup_value(a)?;
            if name == "class" {
                class_at = Some(parts.len());
            }
            if let Some(rest) = name.strip_prefix("aria-") {
                aria.push((rest.replace('-', "_"), value));
                continue;
            }
            let prop = name.replace('-', "_");
            // A typo of one of Helix's own props would quietly become an HTML attribute, and a
            // `clik="save"` button would do nothing at all. A name one edit from a prop, that is
            // not HTML's own, is refused with the prop it nearly is; a React spelling of an HTML
            // attribute (`tabIndex`, `htmlFor`) is given HTML's.
            if !MARKUP_PROPS.contains(&prop.as_str())
                && !lower.starts_with("data-")
                && !HTML_ATTRS.contains(&lower.as_str())
                && let Some(p) = crate::error::suggest(&prop, &MARKUP_PROPS)
            {
                return Err(HelixError::new(format!("`{name}` is not an attribute; did you mean `{}`?", p.replace('_', "-")), a.line, a.col)
                    .hint("Helix's props are `class`, `click`, `input`, `change`, `submit`, `key`, `value` and the gesture and motion names; anything else becomes an HTML attribute as written."));
            }
            let html = if name == "htmlFor" { Some("for") } else { HTML_ATTRS.iter().copied().find(|h| *h == lower) };
            if let Some(h) = html.filter(|_| name.chars().any(|c| c.is_ascii_uppercase())) {
                return Err(HelixError::new(format!("the attribute is `{h}`"), a.line, a.col)
                    .hint(format!("markup uses HTML's names, all lower case: `{h}=…`, not React's `{name}`.")));
            }
            // On a form control, `value` is the HTML value (what an input shows); anywhere
            // else it is what a click sends, as in `ui.el`.
            if prop == "value" && MARKUP_VALUE_TAGS.contains(&tag) {
                raw.push(("value".into(), value));
            } else if MARKUP_PROPS.contains(&prop.as_str()) {
                parts.push(RecordPart::Field(prop, value));
            } else {
                raw.push((name.to_string(), value));
            }
        }
        if !toggles.is_empty() {
            match class_at {
                Some(i) => {
                    if let RecordPart::Field(_, base) = &mut parts[i] {
                        let b = std::mem::replace(base, Expr::Missing);
                        *base = Expr::Array(std::iter::once(b).chain(toggles).collect());
                    }
                }
                // With a spread, the spread's own `class` would be replaced without a word.
                None if spread => {
                    return Err(HelixError::new(format!("`class:…` on `<{tag}>` with a spread needs a `class` attribute too"), line, col)
                        .hint("write the base classes as `class=\"…\"` (or `class={props.class}`) so the toggles have something to add to."));
                }
                None => parts.push(RecordPart::Field("class".into(), Expr::Array(toggles))),
            }
        }
        if !aria.is_empty() {
            parts.push(RecordPart::Field("aria".into(), Expr::Record(aria)));
        }
        if !raw.is_empty() {
            let named = raw.iter().all(|(k, _)| {
                k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            let attrs = if named {
                Expr::Record(raw)
            } else {
                Expr::Method {
                    recv: Box::new(Expr::Array(raw.into_iter().map(|(k, v)| Expr::Tuple(vec![Expr::Str(k), v])).collect())),
                    name: "to_dict".into(),
                    args: vec![],
                    named: vec![],
                    ufcs: None,
                    line,
                    col,
                }
            };
            parts.push(RecordPart::Field("attrs".into(), attrs));
        }
        let props = if spread {
            if !matches!(parts.first(), Some(RecordPart::Spread(_))) {
                parts.insert(0, RecordPart::Spread(Expr::Record(vec![])));
            }
            Expr::RecordUpdate { parts, line, col }
        } else {
            Expr::Record(
                parts
                    .into_iter()
                    .map(|p| match p {
                        RecordPart::Field(k, v) => (k, v),
                        RecordPart::Spread(v) => (String::new(), v),
                    })
                    .collect(),
            )
        };
        let kids = self.markup_children(children)?;
        Ok(Expr::Record(vec![
            ("tag".into(), Expr::Str(tag.to_string())),
            ("props".into(), props),
            ("children".into(), Expr::Array(kids)),
        ]))
    }

    /// `<Card title="Plan" />` calls `card(title: "Plan")` (or `Card`, if that is the name
    /// defined); `<ui.Button …/>` calls `ui.button(…)`. Attributes are named arguments, and
    /// children are the argument `children`.
    fn markup_component(
        &mut self,
        tag: &str,
        attrs: &[crate::markup::MAttr],
        children: &[crate::markup::MNode],
        line: usize,
        col: usize,
    ) -> Result<Expr, HelixError> {
        use crate::markup::MValue;
        let mut named: Vec<(String, Expr)> = Vec::new();
        for a in attrs {
            if let MValue::Spread(_) = a.value {
                return Err(HelixError::new(format!("`<{tag}>` is a function, so it takes named arguments, not a spread"), a.line, a.col)
                    .hint("pass the record as one argument (`props={p}`), or spread it onto an element."));
            }
            if a.name.starts_with("class:") {
                return Err(HelixError::new(format!("`{}` belongs on an element; `<{tag}>` is a function", a.name), a.line, a.col)
                    .hint("pass the condition as an argument, or put `class:…` on the element the function returns."));
            }
            let name = a.name.replace('-', "_");
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(HelixError::new(format!("`{}` is not a parameter name", a.name), a.line, a.col)
                    .hint(format!("an attribute of `<{tag}>` is one of its parameters, by name.")));
            }
            if named.iter().any(|(k, _)| *k == name) {
                return Err(HelixError::new(format!("`{name}` is given twice on `<{tag}>`"), a.line, a.col));
            }
            let v = self.markup_value(a)?;
            named.push((name, v));
        }
        if !children.is_empty() {
            let kids = self.markup_children(children)?;
            if named.iter().any(|(k, _)| k == "children") {
                return Err(HelixError::new(format!("`<{tag}>` has children and a `children` attribute"), line, col));
            }
            named.push(("children".into(), Expr::Array(kids)));
        }
        let parts: Vec<&str> = tag.split('.').collect();
        match parts.as_slice() {
            [module, f] => Ok(Expr::Method {
                recv: Box::new(Expr::Ident { name: (*module).to_string(), line, col }),
                name: snake_case(f),
                args: vec![],
                named,
                ufcs: None,
                line,
                col,
            }),
            [f] => {
                let name = if self.fn_sigs.contains_key(*f) { (*f).to_string() } else { snake_case(f) };
                if !self.fn_sigs.contains_key(&name) && !crate::registry::is_builtin_name(&name) {
                    let known: Vec<&str> = self.fn_sigs.keys().map(|k| k.as_str()).collect();
                    let hint = match crate::error::suggest(&name, &known) {
                        Some(n) => format!("did you mean `<{}>`, the function `{n}`?", pascal_case(&n)),
                        None if crate::uidocs::find(&name).is_some() => {
                            format!("`{name}` is in the app library: write `<ui.{tag}>` (with `import std.ui`).")
                        }
                        None => "a capitalised tag is a component: a function whose parameters are the tag's \
                                 attributes (and `children`). A library's is written with its name: `<ui.Badge />`."
                            .to_string(),
                    };
                    return Err(HelixError::new(format!("`<{tag}>` needs a function `{name}`"), line, col).hint(hint));
                }
                let args = self.resolve_call_args(&name, vec![], named, line, col)?;
                Ok(Expr::Call { name, args, line, col })
            }
            _ => Err(HelixError::new(format!("`<{tag}>` has more than one `.`"), line, col)
                .hint("a component tag is a function, `<Card>`, or a module's function, `<ui.Card>`.")),
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
            Tok::Markup(node) => {
                self.advance();
                self.markup(&node)
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
                            let (mut e, hole_do_bindings, hole_loops, hole_gone) = parse_expression(&src, self)
                                .map_err(|err| HelixError { line: l, col: c, ..err })?;
                            self.loops += hole_loops;
                            // Where the checker will report them: at the string, as the
                            // relocation below puts every name in the hole.
                            self.gone_uses.extend(hole_gone.into_iter().map(|(n, _, _, why)| (n, l, c, why)));
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
                // `ys = for x in xs { … }` — a loop where a value goes (ADR 0066).
                if name == "for" && self.for_ahead() {
                    return Err(HelixError::new(
                        "a `for` loop is a statement; to build a collection use `xs.map(…)`",
                        l,
                        c,
                    )
                    .hint(
                        "a loop has no value: it changes the names bound before it. `ys = xs.map(x => x * 2)` \
                         builds an array, `xs.filter(p)` keeps some of one, and `xs.reduce(init, (acc, x) => …)` \
                         folds one into a single value.",
                    ));
                }
                self.advance();
                if self.loop_depth > 0
                    && matches!(name.as_str(), "break" | "continue")
                    && !self.is_bound(&name)
                    && !self.fn_sigs.contains_key(&name)
                    && !self.selected_imports.contains(&name)
                {
                    return Err(no_loop_exit(&name, l, c));
                }
                self.note_use(&name, l, c);
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
                // The bindings are a scope of their own (ADR 0066): bound for the body, and
                // each for the ones after it.
                self.push_scope([]);
                let parsed = self.let_rest();
                self.pop_scope();
                parsed
            }
            Tok::Do => {
                self.push_scope([]);
                let block = self.do_block();
                self.pop_scope();
                block
            }
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
                if matches!(self.peek(), Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq) {
                    let (al, ac) = self.pos();
                    return Err(HelixError::new("an `if` cannot hold an assignment", al, ac).hint(
                        "`if` is an expression: both branches are values. A conditional update binds the \
                         name to the `if` — `total = if x > 0 then total + x else total`.",
                    ));
                }
                if self.markup && !matches!(self.peek(), Tok::Else) {
                    return Ok(Expr::If {
                        cond: Box::new(cond),
                        then_branch: Box::new(then_branch),
                        else_branch: Box::new(Expr::Missing),
                        line: l,
                        col: c,
                    });
                }
                self.eat(&Tok::Else, "after the `then` branch").map_err(|e| {
                    e.hint("`if` is an expression, so it needs an `else` that yields a value. For a guard, \
                            `if n < 1 then raise(\"n must be positive\") else missing` (or `assert(n >= 1, \"…\")`); \
                            inside markup's `{…}` the `else` may be left out, and renders nothing.")
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
                    // What the pattern binds is bound for the guard and the result.
                    let mut binds = Vec::new();
                    pattern_names(&pattern, &mut binds);
                    self.push_scope(binds);
                    let arm = self.match_arm_rest();
                    self.pop_scope();
                    let (guard, body) = arm?;
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
                let (bl, bc) = self.pos();
                self.advance();
                let mut elems = Vec::new();
                // `[head, ...rest, last]`: a spread puts a list's items in place. It is the
                // same `...` records and destructuring already use, and desugars to `concat`,
                // so no engine sees it. `spreads[i]` says whether `elems[i]` was one.
                let mut spreads: Vec<bool> = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        let spread = matches!(self.peek(), Tok::DotDotDot);
                        if spread {
                            self.advance();
                        }
                        spreads.push(spread);
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
                if !spreads.contains(&true) {
                    return Ok(Expr::Array(elems));
                }
                // Runs of plain items become arrays; spreads stay as they are; then `concat`.
                let mut parts: Vec<Expr> = Vec::new();
                let mut run: Vec<Expr> = Vec::new();
                for (e, sp) in elems.into_iter().zip(spreads) {
                    if sp {
                        if !run.is_empty() {
                            parts.push(Expr::Array(std::mem::take(&mut run)));
                        }
                        parts.push(e);
                    } else {
                        run.push(e);
                    }
                }
                if !run.is_empty() {
                    parts.push(Expr::Array(run));
                }
                // A lone spread `[...xs]` is a copy: `[].concat(xs)`, still checked as a list.
                let mut it = parts.into_iter();
                let first = it.next().unwrap_or(Expr::Array(vec![]));
                let start = if matches!(first, Expr::Array(_)) { first } else {
                    Expr::Method { recv: Box::new(Expr::Array(vec![])), name: "concat".into(), args: vec![first], named: vec![], ufcs: None, line: bl, col: bc }
                };
                Ok(it.fold(start, |acc, p| Expr::Method {
                    recv: Box::new(acc),
                    name: "concat".into(),
                    args: vec![p],
                    named: vec![],
                    ufcs: None,
                    line: bl,
                    col: bc,
                }))
            }
            Tok::LBrace => {
                // record literal: `{ name: expr, age: expr }`, or a record UPDATE that
                // starts with a spread: `{ ...base, status: 500 }`.
                let (l, c) = self.pos();
                let may_pin = self.pinned_keys_at == Some(self.pos);
                self.advance();
                // A leading `...expr` makes this a record update: its parts — spreads and
                // named fields — apply in written order, a later one winning. The spread
                // comes first; any number may follow (field build, 1.49: `{...ADULTS,
                // ...NEWEST}` merges two query fragments, which had no spelling at all).
                if matches!(self.peek(), Tok::DotDotDot) {
                    self.advance();
                    let mut parts: Vec<crate::ast::RecordPart> =
                        vec![crate::ast::RecordPart::Spread(self.expr()?)];
                    let mut named: Vec<String> = Vec::new();
                    while matches!(self.peek(), Tok::Comma) {
                        self.advance();
                        if matches!(self.peek(), Tok::RBrace) {
                            break; // trailing comma
                        }
                        if matches!(self.peek(), Tok::DotDotDot) {
                            self.advance();
                            parts.push(crate::ast::RecordPart::Spread(self.expr()?));
                            continue;
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
                        // duplicate — a spread's fields are not in it.
                        if named.contains(&key) {
                            return Err(HelixError::new(
                                format!("duplicate field `{}` in record update", key),
                                kl,
                                kc,
                            )
                            .hint("each field may be given once; the later value would silently win."));
                        }
                        self.eat(&Tok::Colon, &format!("after field `{}`", key))?;
                        named.push(key.clone());
                        parts.push(crate::ast::RecordPart::Field(key, self.expr()?));
                    }
                    self.eat(&Tok::RBrace, "to close the record update")?;
                    return Ok(Expr::RecordUpdate { parts, line: l, col: c });
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
                        // A QUOTED key in a record brace names a field whose spelling is not an
                        // identifier — `{city: "oslo", "age >=": 18}`, the query-builder shape
                        // where the operator rides in the key (field build, 1.27.4). The brace
                        // stays a record (its first key was bare): written order kept, the field
                        // reachable as `rec.get("age >=")`, printed back quoted. It used to be
                        // refused as a mixing of the two brace forms, which made adding one
                        // operator re-spell every key of the clause and turn it into a Dict — a
                        // different type with a different iteration order. A bare key in a DICT
                        // brace is still refused (a quoted key is only ever a key; a bare one in
                        // that position is the ambiguity the two forms exist to avoid).
                        let key = if let Tok::Str(s) = self.peek() {
                            let s = s.clone();
                            self.advance();
                            s
                        } else if matches!(self.peek(), Tok::At) {
                            // `{@name: e}` — the column literally called `name` (ADR 0054).
                            // Stored as `@name`: no identifier starts with `@`, so the key
                            // cannot collide with a bare one, and the frame verb strips it.
                            if !may_pin {
                                return Err(HelixError::new(
                                    "an `@name` key only means something in a DataFrame's `with` or `agg`",
                                    kl,
                                    kc,
                                )
                                .hint(
                                    "`df.with({@name: e})` defines the column literally called `name`; \
                                     a record's own field is written without the `@`.",
                                ));
                            }
                            self.advance();
                            let name = self.ident_name("after `@` (a column name)")?;
                            format!("@{name}")
                        } else {
                            self.member_name("as a record field name")?
                        };
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

// --- `for` loops and compound assignment (ADR 0066). ----------------------------------------
//
// `for x in xs { total += x }` is notation. The parser writes the fold it stands for —
//
//     total = xs.reduce(total, (total, x) => total + x)
//
// and, for a body that assigns several names bound before the loop,
//
//     $forout = xs.reduce((total, n), ($st, x) => do {
//       total = $st[0]
//       n = $st[1]
//       total = total + x
//       n = n + 1
//       (total, n)
//     })
//     total = $forout[0]
//     n = $forout[1]
//
// — so the checker, the three engines and the JIT see a `reduce`, a lambda and a `do` block,
// which they already agree about. What the parser has to know, and nothing else does, is
// which names the body assigns were BOUND BEFORE THE LOOP: those are the fold's state, and
// they are what `scopes` is for.

/// One level of [`Parser::scopes`].
#[derive(Clone, Default)]
struct Scope {
    names: std::collections::HashSet<String>,
    /// Names a loop that has ended bound for one iteration only, and so are not bound here:
    /// a use of one is the unknown name it always was, and the checker's error says why
    /// ([`loop_local_hint`]).
    gone: HashMap<String, Gone>,
}

/// A read of a name a finished loop had bound: the name, where, and why it is gone.
type GoneUse = (String, usize, usize, Gone);

/// Why a name a loop bound does not exist after it.
#[derive(Clone, Copy, PartialEq)]
enum Gone {
    /// First bound by a line of the body.
    Body,
    /// The loop's own variable.
    Variable,
}

thread_local! {
    /// Where the file read a name that only a finished loop had bound: (name, line, column).
    /// Written as the parser passes each such use, read by the checker when — and only when —
    /// it is already refusing that name as not defined, and emptied when a check ends, so an
    /// entry never outlives the program it was read from.
    static GONE_USES: std::cell::RefCell<HashMap<(String, usize, usize), Gone>> =
        std::cell::RefCell::new(HashMap::new());
}

/// The hint for an unknown name that a `for` loop had bound (ADR 0066), if this use of it —
/// by name and position — is one.
pub(crate) fn loop_local_hint(name: &str, line: usize, col: usize) -> Option<&'static str> {
    let why = GONE_USES.with(|t| t.borrow().get(&(name.to_string(), line, col)).copied())?;
    Some(gone_hint(why))
}

fn gone_hint(why: Gone) -> &'static str {
    match why {
        Gone::Body => "it was first bound inside the `for` body, so it does not outlive an iteration; bind it before the loop to carry it out.",
        Gone::Variable => "it is the `for` loop's own variable, so it does not outlive an iteration; bind a name before the loop and assign to it in the body to carry a value out.",
    }
}

/// Forget every use [`loop_local_hint`] knows: the check they were recorded for is over.
pub(crate) fn forget_loop_locals() {
    GONE_USES.with(|t| t.borrow_mut().clear());
}

/// The start of the sentence a `for` over something that is not an array is refused with.
/// The checker finishes it from the receiver's type ([`for_needs_iterable`]) and the fold
/// the parser writes finishes it from the value's ([`Parser::fold_of`]), in the same words.
const FOR_NEEDS: &str = "`for` needs something to iterate — an Array, a `range(…)`, `d.items()` — and this is ";

/// "`for` needs something to iterate — … — and this is an Int": the checker's refusal of a
/// loop over a value whose type is known and is not an array.
pub(crate) fn for_needs_iterable(type_name: &str, line: usize, col: usize) -> HelixError {
    let what = if type_name == "Missing" { "`missing`".to_string() } else { crate::value::with_article(type_name) };
    HelixError::new(format!("{FOR_NEEDS}{what}"), line, col).hint(FOR_NEEDS_HELP)
}

/// The help under that sentence, from the checker and from the fold alike.
const FOR_NEEDS_HELP: &str = "a loop walks a sequence. To count, write `for i in range(n) { … }`; a dict's or a record's \
     entries are `for (k, v) in d.items() { … }`, and a string's characters `for ch in s.chars() { … }`.";

/// Whether a `reduce`'s receiver is the sequence of a `for` loop: the name the parser binds
/// it to. No source can spell it.
pub(crate) fn is_loop_source(recv: &Expr) -> bool {
    matches!(recv, Expr::Ident { name, .. } if name.starts_with(LOOP_SOURCE))
}

/// Whether an expression is the fold a `for` loop was written as — for a tool that reads
/// the tree and has to speak of the loop the person wrote (`helix emit rust`).
pub(crate) fn is_loop_fold(e: &Expr) -> bool {
    match e {
        Expr::Let { bindings, .. } => bindings.first().is_some_and(|(n, _)| n.starts_with(LOOP_SOURCE)),
        Expr::Method { name, ufcs, .. } => name == "reduce" && ufcs.as_deref() == Some(LOOP_FOLD),
        _ => false,
    }
}

/// The branch of a loop's fold that refuses a sequence that is not one — the parser's own
/// `raise`, which is not something the function the loop is in does. `helix effects` reads
/// past it: a function is not less reproducible for holding a loop.
pub(crate) fn loop_refusal(e: &Expr) -> Option<&Expr> {
    match e {
        Expr::Let { bindings, body, .. } if bindings.first().is_some_and(|(n, _)| n.starts_with(LOOP_SOURCE)) => {
            match &**body {
                Expr::If { else_branch, .. } => Some(else_branch),
                _ => None,
            }
        }
        _ => None,
    }
}

const LOOP_SOURCE: &str = "$forin";
/// What a loop's `reduce` carries where a method call names the free function it falls
/// back to (`Expr::Method::ufcs`). No function has this name, so the fold falls back to
/// nothing — a `fn reduce` the file declares is not a loop's — and the call is known for a
/// loop's by anything that reads the tree ([`is_loop_fold`]).
const LOOP_FOLD: &str = "$for";
const LOOP_RESULT: &str = "$forout";

/// `break` and `continue`, which a fold has no place for.
fn no_loop_exit(word: &str, line: usize, col: usize) -> HelixError {
    let e = HelixError::new(format!("a `for` loop has no `{word}`"), line, col);
    if word == "break" {
        e.hint("a loop runs over the whole sequence, so cut the sequence first: `for x in xs.take_while(x => x < limit) { … }`.")
    } else {
        e.hint("a loop runs its body for every element, so choose the elements first: `for x in xs.filter(x => x > 0) { … }`.")
    }
}

/// Which of `type_of`, `raise` and `range` this file may bind to something of its own: a
/// `fn` of the name, or the name anywhere it is not being called. The fold a loop is written
/// as calls the first two to refuse a sequence that is not one, and reads `range(…)` as an
/// array without asking; a file that has its own does without both, and a loop over a
/// non-array there is refused by `reduce`, in `reduce`'s words.
fn rebound_names(toks: &[Token]) -> std::collections::HashSet<&'static str> {
    let mut out = std::collections::HashSet::new();
    for (i, t) in toks.iter().enumerate() {
        let Tok::Ident(n) = &t.tok else { continue };
        let Some(name) = ["type_of", "raise", "range"].into_iter().find(|k| k == n) else { continue };
        let declared = i > 0 && matches!(toks[i - 1].tok, Tok::Fn);
        let member = i > 0 && matches!(toks[i - 1].tok, Tok::Dot);
        let called = matches!(toks.get(i + 1).map(|t| &t.tok), Some(Tok::LParen));
        if declared || !(called || member) {
            out.insert(name);
        }
    }
    out
}

/// The names a match pattern binds.
fn pattern_names(p: &crate::ast::Pattern, out: &mut Vec<String>) {
    use crate::ast::Pattern;
    match p {
        Pattern::Bind(n) => out.push(n.clone()),
        Pattern::Tuple(ps) | Pattern::Or(ps) => ps.iter().for_each(|p| pattern_names(p, out)),
        Pattern::Record(fields) => fields.iter().for_each(|(_, p)| pattern_names(p, out)),
        _ => {}
    }
}

/// A loop's pattern: `x`, or the positions `[a, b]` / `(a, b)` of each element.
enum LoopPattern {
    Name(String),
    Positions { names: Vec<(String, usize, usize)>, rest: Option<(String, usize, usize)> },
}

/// A loop, as the lines its block gets: the fold — bound to the one name it carries, or to
/// a temporary — and, when it carries several, one read per name. `carried` is those names,
/// each where the body first assigns it.
struct LoopOut {
    bindings: Vec<(String, Expr)>,
    carried: Vec<(String, usize, usize)>,
}

impl Parser {
    fn tok(&self, at: usize) -> &Tok {
        &self.toks[at.min(self.toks.len() - 1)].tok
    }

    fn push_scope<I: IntoIterator<Item = String>>(&mut self, names: I) {
        self.scopes.push(Scope { names: names.into_iter().collect(), gone: HashMap::new() });
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    /// `name` is bound from here on, in the innermost scope. A generated name is not a name.
    fn bind(&mut self, name: &str) {
        if name.starts_with('$') {
            return;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.gone.remove(name);
            scope.names.insert(name.to_string());
        }
    }

    fn bind_all(&mut self, bindings: &[(String, Expr)]) {
        for (name, _) in bindings {
            self.bind(name);
        }
    }

    /// Whether a binding of `name` is in scope here.
    fn is_bound(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.names.contains(name))
    }

    /// Whether `name` may mean something here that this parser cannot see bound: a `fn`,
    /// or a name an import brings.
    fn is_known(&self, name: &str) -> bool {
        self.is_bound(name)
            || self.fn_sigs.contains_key(name)
            || self.selected_imports.contains(name)
            || self.glob_import
    }

    /// Why `name` is not bound here although a loop bound it, if that is the case.
    fn gone(&self, name: &str) -> Option<Gone> {
        for scope in self.scopes.iter().rev() {
            if scope.names.contains(name) {
                return None;
            }
            if let Some(why) = scope.gone.get(name) {
                return Some(*why);
            }
        }
        None
    }

    /// A read of `name` at this position: remembered if only a finished loop had bound it.
    fn note_use(&mut self, name: &str, line: usize, col: usize) {
        if let Some(why) = self.gone(name) {
            self.gone_uses.push((name.to_string(), line, col, why));
        }
    }

    /// A function's body and its `where` clause (ADR 0035), in the function's scope.
    ///
    /// A `where` binding is written AFTER the body it is bound for, and a loop in the body
    /// that assigns one must know the name is bound — or it would treat it as local to an
    /// iteration. So the clause's names are read ahead ([`Parser::where_names_ahead`]) and
    /// are in scope before the body is parsed.
    fn fn_body(&mut self) -> Result<Expr, HelixError> {
        for name in self.where_names_ahead() {
            self.bind(&name);
        }
        let body = self.expr()?;
        // ADR 0035: an optional `where` clause — the scaffolding AFTER the point, scoped
        // to this function. `where` stays an ordinary identifier everywhere else (frames
        // own a `.where(...)` verb), so the gate is the exact shape `where <name> =` after
        // the body, optionally across newlines — no legal program parses that today.
        // Desugars to the `let … in` the body could have written: zero new engine
        // surface, parity by construction.
        self.maybe_where_clause(body)
    }

    /// The names the `where` clause of the function whose body starts at the cursor binds,
    /// found by looking: the clause opens at the first `where NAME =` (or `where {…} =`,
    /// `where [… ] =`) outside every bracket — on the body's last line or after it — which
    /// is the shape [`Parser::maybe_where_clause`] gates on and nothing inside an
    /// expression can have, `=` not being an operator.
    fn where_names_ahead(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut depth = 0usize;
        let mut k = self.pos;
        // To the clause, or to the end of the statement.
        loop {
            match self.tok(k) {
                Tok::Eof => return names,
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth = depth.saturating_sub(1),
                Tok::Newline if depth == 0 => {
                    let mut n = k;
                    while matches!(self.tok(n), Tok::Newline) {
                        n += 1;
                    }
                    if !self.where_opens_at(n) {
                        return names;
                    }
                    k = n;
                    break;
                }
                Tok::Ident(_) if depth == 0 && self.where_opens_at(k) => break,
                _ => {}
            }
            k += 1;
        }
        // The bindings: a name before each `=` outside brackets, or a binder's names.
        k += 1;
        let mut at_binding = true;
        loop {
            match self.tok(k) {
                Tok::Eof => break,
                Tok::Newline if depth == 0 => break,
                Tok::Ident(n) if depth == 0 && at_binding && matches!(self.tok(k + 1), Tok::Eq) => {
                    names.push(n.clone());
                    at_binding = false;
                }
                Tok::LBrace | Tok::LBracket if depth == 0 && at_binding => {
                    // `{field, field: binder}` / `[a, b, ...rest]`: the binder is the name
                    // not followed by a colon.
                    let mut j = k + 1;
                    while !matches!(self.tok(j), Tok::RBrace | Tok::RBracket | Tok::Eof) {
                        if let Tok::Ident(n) = self.tok(j)
                            && !matches!(self.tok(j + 1), Tok::Colon)
                        {
                            names.push(n.clone());
                        }
                        j += 1;
                    }
                    k = j;
                    at_binding = false;
                }
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth = depth.saturating_sub(1),
                Tok::Comma if depth == 0 => at_binding = true,
                _ => {}
            }
            k += 1;
        }
        names
    }

    /// Whether a `where` clause opens at token `k`.
    fn where_opens_at(&self, k: usize) -> bool {
        matches!(self.tok(k), Tok::Ident(w) if w == "where")
            && ((matches!(self.tok(k + 1), Tok::Ident(_)) && matches!(self.tok(k + 2), Tok::Eq))
                || (matches!(self.tok(k + 1), Tok::LBrace) && self.record_binder_from(k + 1))
                || (matches!(self.tok(k + 1), Tok::LBracket) && self.array_binder_from(k + 1)))
    }

    /// Whether the cursor is at `for PATTERN in` — a loop. `for` is a name everywhere else
    /// (it is an HTML attribute in markup, and a program may call something `for`), and
    /// `in` is reserved, so nothing that parsed before starts this way.
    fn for_ahead(&self) -> bool {
        matches!(self.peek(), Tok::Ident(n) if n == "for")
            && self.loop_pattern_end(self.pos + 1).is_some_and(|k| matches!(self.tok(k), Tok::In))
    }

    /// The token after the loop pattern that starts at `at`: a name, or names in `[…]`
    /// (a `...rest` among them) or in `(…)`.
    fn loop_pattern_end(&self, at: usize) -> Option<usize> {
        let close = match self.tok(at) {
            Tok::Ident(_) => return Some(at + 1),
            Tok::LBracket => Tok::RBracket,
            Tok::LParen => Tok::RParen,
            _ => return None,
        };
        let mut k = at + 1;
        loop {
            match self.tok(k) {
                Tok::Ident(_) | Tok::Comma | Tok::DotDotDot | Tok::Newline => k += 1,
                t if *t == close => return Some(k + 1),
                _ => return None,
            }
        }
    }

    /// A statement that opens with `for (` or `foreach` and is not a loop: another
    /// language's. Named here, where the next thing the parser could say is that a call's
    /// argument list is not closed. A program that has its own `for` is left to mean it.
    fn foreign_for(&self) -> Option<HelixError> {
        let Tok::Ident(word) = self.peek() else { return None };
        let (l, c) = self.pos();
        let next = self.tok(self.pos + 1);
        let opens_like_a_loop = match word.as_str() {
            // `for (i in 1:3)`, `for (const x of xs)`, `for (i = 0; …)`: a parenthesis that
            // holds more than a call's arguments could.
            "for" => {
                matches!(next, Tok::LParen)
                    && matches!(self.tok(self.pos + 2), Tok::Ident(_))
                    && matches!(self.tok(self.pos + 3), Tok::In | Tok::Ident(_) | Tok::Eq)
            }
            // `foreach x in xs`, `foreach (x in xs)`.
            "foreach" => {
                let at = if matches!(next, Tok::LParen) { self.pos + 2 } else { self.pos + 1 };
                matches!(self.tok(at), Tok::Ident(_)) && matches!(self.tok(at + 1), Tok::In)
            }
            _ => false,
        };
        if !opens_like_a_loop || self.is_known(word) {
            return None;
        }
        let what = if word == "foreach" {
            "Helix spells `foreach` as `for`"
        } else {
            "a `for` loop takes no parentheses around its header"
        };
        Some(HelixError::new(what, l, c).hint(FOR_SHAPE))
    }

    /// `for PATTERN in EXPR { BODY }`, as the lines of the block it is in. `top` says the
    /// loop is a top-level statement (or a line of one such loop's body): there the names it
    /// carries are `mut` globals, which its body rebinds as locals of the fold on purpose —
    /// so they are not among the `do {}` bindings checked against the globals
    /// ([`reject_do_binding_over_mut_global`]). Everywhere else they are.
    fn for_loop(&mut self, top: bool) -> Result<LoopOut, HelixError> {
        let (fl, fc) = self.pos();
        // A loop in a loop does not pass through `unary`, where nesting is counted, and
        // each one is several levels of the tree that is built: count it as that many, so
        // what bounds the parser still bounds every pass that walks its output.
        let saved = self.depth;
        for _ in 0..8 {
            self.deepen()?;
        }
        self.advance(); // `for`
        let pattern = self.loop_pattern()?;
        self.eat(&Tok::In, "after the loop's pattern")?;
        let (sl, sc) = self.pos();
        let source = self.expr()?;
        if !matches!(self.peek(), Tok::LBrace) {
            let (l, c) = self.pos();
            let found = self.peek().describe();
            let e = HelixError::new(format!("expected `{{` to open the `for` body, found {found}"), l, c);
            return Err(match self.peek() {
                Tok::Colon => e.hint(
                    "the body of a loop goes in braces, not after a colon: `for x in xs { total += x }`. \
                     Indentation means nothing to Helix.",
                ),
                Tok::Do => e.hint("the body of a loop goes in braces, with no `do`: `for x in xs { total += x }`."),
                _ => e.hint(FOR_SHAPE),
            });
        }
        self.advance(); // `{`
        self.loops += 1;
        let number = self.loops;
        let variable: Vec<String> = match &pattern {
            LoopPattern::Name(n) => vec![n.clone()],
            LoopPattern::Positions { names, rest } => names.iter().chain(rest).map(|(n, ..)| n.clone()).collect(),
        };
        self.push_scope(variable.iter().cloned());
        self.loop_depth += 1;
        let body = self.loop_body(top);
        self.loop_depth -= 1;
        let scope = self.scopes.pop().unwrap_or_default();
        let (lines, carried) = body?;
        // What the body bound for itself is gone, and a later read of it is told so.
        if let Some(outer) = self.scopes.last_mut() {
            for name in &scope.names {
                if name == "_" || outer.names.contains(name) {
                    continue;
                }
                let why = if variable.contains(name) { Gone::Variable } else { Gone::Body };
                outer.gone.insert(name.clone(), why);
            }
        }
        let fold = self.fold_of((fl, fc), (sl, sc), number, pattern, source, lines, &carried);
        let ident = |name: &str, l: usize, c: usize| Expr::Ident { name: name.to_string(), line: l, col: c };
        let bindings = match carried.as_slice() {
            // Run for its effects: the fold's `()` goes nowhere.
            [] => vec![(format!("{LOOP_RESULT}{number}"), fold)],
            // One name is the state itself.
            [(name, ..)] => vec![(name.clone(), fold)],
            // Several are a tuple, read back by position.
            many => {
                let result = self.result_name(number, top);
                let mut out = vec![(result.clone(), fold)];
                for (i, (name, l, c)) in many.iter().enumerate() {
                    let read = Expr::Index {
                        recv: Box::new(ident(&result, *l, *c)),
                        index: Box::new(Expr::Int(i as i64)),
                        line: *l,
                        col: *c,
                    };
                    out.push((name.clone(), read));
                }
                out
            }
        };
        self.depth = saved;
        Ok(LoopOut { bindings, carried })
    }

    /// The name a loop's tuple of results is bound to. In a block any name does, since its
    /// reads are the next lines; at the top level it is a global, so it must be one no
    /// earlier statement — of this file, or of the session before it — has taken.
    fn result_name(&mut self, number: usize, top: bool) -> String {
        let mut n = number;
        loop {
            let name = format!("{LOOP_RESULT}{n}");
            if !top || !self.scopes[0].names.contains(&name) {
                if top {
                    self.scopes[0].names.insert(name.clone());
                }
                return name;
            }
            n += 1000;
        }
    }

    /// A loop as top-level statements: the first returned, the rest queued as a
    /// destructuring statement's are. Each carried name is assigned where the body first
    /// assigns it, so a name that is not `mut` is refused there, as any reassignment is.
    fn loop_statements(&mut self, out: LoopOut, line: usize, col: usize) -> Stmt {
        if out.carried.is_empty() {
            // Nothing to bind: the fold is a statement by itself.
            let fold = out.bindings.into_iter().next().map(|(_, e)| e).unwrap_or(Expr::Tuple(Vec::new()));
            return Stmt::Expr(fold);
        }
        let several = out.carried.len() > 1;
        let mut at = out.carried.iter().map(|(_, l, c)| (*l, *c));
        let mut stmts = out.bindings.into_iter().enumerate().map(|(i, (name, value))| {
            let (l, c) = if several && i == 0 { (line, col) } else { at.next().unwrap_or((line, col)) };
            Stmt::Assign { name, mutable: false, exported: false, value, line: l, col: c }
        });
        // `carried` is not empty, so there is a first; the fallback is never built.
        let first = stmts.next().unwrap_or(Stmt::Expr(Expr::Tuple(Vec::new())));
        let rest: Vec<Stmt> = stmts.collect();
        self.pending.extend(rest);
        first
    }

    /// The pattern between `for` and `in`. [`Parser::for_ahead`] has seen its shape.
    fn loop_pattern(&mut self) -> Result<LoopPattern, HelixError> {
        let (l, c) = self.pos();
        let close = match self.peek() {
            Tok::LBracket => Tok::RBracket,
            Tok::LParen => Tok::RParen,
            _ => return Ok(LoopPattern::Name(self.ident_name("as the loop's variable")?)),
        };
        let shape = "a loop's pattern is a name, or the positions of each element: `for (k, v) in d.items() { … }`, `for [first, ...rest] in rows { … }`.";
        let bracketed = close == Tok::RBracket;
        self.advance();
        let mut names: Vec<(String, usize, usize)> = Vec::new();
        let mut rest: Option<(String, usize, usize)> = None;
        let mut commas = 0;
        loop {
            self.skip_newlines();
            if *self.peek() == close {
                break;
            }
            let (nl, nc) = self.pos();
            if matches!(self.peek(), Tok::DotDotDot) {
                if !bracketed || rest.is_some() {
                    return Err(HelixError::new("a `...rest` goes last, in a bracketed pattern", nl, nc).hint(shape));
                }
                self.advance();
                rest = Some((self.ident_name("after `...` — the name for the rest")?, nl, nc));
            } else if rest.is_some() {
                return Err(HelixError::new("`...rest` must come last in a pattern", nl, nc).hint(shape));
            } else {
                names.push((self.ident_name("in the loop's pattern")?, nl, nc));
            }
            self.skip_newlines();
            if matches!(self.peek(), Tok::Comma) {
                self.advance();
                commas += 1;
            }
        }
        self.advance(); // the closer
        if names.is_empty() && rest.is_none() {
            return Err(HelixError::new("a pattern needs at least one name", l, c).hint(shape));
        }
        Self::refuse_twice(names.iter().chain(&rest))?;
        // `for (x) in xs` is `for x in xs`, as `(x) => …` is `x => …`.
        if !bracketed && names.len() == 1 && commas == 0 {
            return Ok(LoopPattern::Name(names.remove(0).0));
        }
        Ok(LoopPattern::Positions { names, rest })
    }

    /// The lines of a loop body, up to and over its `}`: bindings, compound assignments,
    /// expressions run for their effect, and loops. Unlike a `do` block it ends without a
    /// result. Answers the lines as bindings, and the names the body CARRIES — those it
    /// assigns that were bound before the loop — each where it is first assigned.
    #[allow(clippy::type_complexity)]
    fn loop_body(&mut self, top: bool) -> Result<(Vec<(String, Expr)>, Vec<(String, usize, usize)>), HelixError> {
        let mut lines: Vec<(String, Expr)> = Vec::new();
        let mut carried: Vec<(String, usize, usize)> = Vec::new();
        let body = self.scopes.len() - 1;
        self.skip_newlines();
        loop {
            let (bl, bc) = self.pos();
            match self.peek() {
                Tok::RBrace => {
                    self.advance();
                    return Ok((lines, carried));
                }
                Tok::Eof => {
                    return Err(HelixError::new("this `for` body is not closed", bl, bc)
                        .hint("a loop's body ends with `}`: `for x in xs { total += x }`."));
                }
                Tok::Mut => return Err(mut_inside_a_body(bl, bc)),
                Tok::Fn => {
                    return Err(HelixError::new("`fn` cannot be defined inside a `for` body", bl, bc).hint(
                        "`fn` is item-level — define it at the top of the file. For a local \
                         function, bind a lambda instead: `f = (x) => x * 2`.",
                    ));
                }
                _ => {}
            }
            let from = lines.len();
            if self.for_ahead() {
                // A loop in a loop: what it carries, this body assigns.
                let inner = self.for_loop(top)?;
                for (name, l, c) in &inner.carried {
                    self.body_assigns(body, &mut carried, name, *l, *c);
                }
                lines.extend(inner.bindings);
                self.end_of_line("the `for` loop")?;
                continue;
            }
            if let Some(e) = self.foreign_for() {
                return Err(e);
            } else if self.compound_ahead() {
                let (name, value) = self.compound_assignment()?;
                lines.push((name, value));
            } else if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Eq) {
                let name = self.ident_name("")?;
                self.advance(); // `=`
                let value = self.expr()?;
                lines.push((name, value));
            } else if matches!(self.peek(), Tok::LBrace) && self.record_binder_ahead() {
                let tmp = format!("$rec{from}");
                self.destructure_record(&mut lines, tmp)?;
            } else if matches!(self.peek(), Tok::LBracket) && self.array_binder_ahead() {
                let tmp = format!("$arr{from}");
                self.destructure_positions(&mut lines, tmp)?;
            } else if self.at_destructure() {
                return Err(HelixError::new("a pattern inside a block is written in brackets", bl, bc)
                    .hint("`[a, b] = value` — the bare `a, b = value` is a top-level statement."));
            } else {
                // Run for its effect, as a non-final line of a `do` block is.
                let effect = self.expr()?;
                lines.push((format!("$do{from}"), effect));
            }
            // What the line bound: carried if it was bound before the loop, else this
            // iteration's own.
            let bound: Vec<String> =
                lines[from..].iter().map(|(n, _)| n.clone()).filter(|n| !n.starts_with('$')).collect();
            for name in bound {
                self.body_assigns(body, &mut carried, &name, bl, bc);
                if !top {
                    self.do_bindings.push((name, bl, bc));
                }
            }
            self.end_of_line("a line of the `for` body")?;
        }
    }

    /// The body assigns `name`: to the loop's variable or a name of its own, which stays
    /// the iteration's; to a name bound before the loop, which the loop then carries; or
    /// to a new name, which is the iteration's from here.
    fn body_assigns(&mut self, body: usize, carried: &mut Vec<(String, usize, usize)>, name: &str, line: usize, col: usize) {
        if name.starts_with('$') || self.scopes[body].names.contains(name) {
            return;
        }
        if self.scopes[..body].iter().any(|s| s.names.contains(name)) {
            if !carried.iter().any(|(n, ..)| n == name) {
                carried.push((name.to_string(), line, col));
            }
        } else {
            self.scopes[body].gone.remove(name);
            self.scopes[body].names.insert(name.to_string());
        }
    }

    /// A statement inside a block ends at the end of its line, or at the block's `}`.
    fn end_of_line(&mut self, what: &str) -> Result<(), HelixError> {
        match self.peek() {
            Tok::Newline => {
                self.skip_newlines();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            other => {
                let (l, c) = self.pos();
                Err(HelixError::new(format!("expected end of line after {what}, found {}", other.describe()), l, c)
                    .hint("a block has one statement per line: start the next one on a line of its own."))
            }
        }
    }

    /// Whether the cursor is at `NAME +=` (or `-=`, `*=`, `/=`).
    fn compound_ahead(&self) -> bool {
        matches!(self.peek(), Tok::Ident(_))
            && matches!(self.peek_at(1), Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq)
    }

    /// `name += value` — the binding `name = name + value` (ADR 0066), as its name and its
    /// value. Refused on a name that is not bound: there is nothing to add to.
    fn compound_assignment(&mut self) -> Result<(String, Expr), HelixError> {
        let (nl, nc) = self.pos();
        let name = self.ident_name("")?;
        let (ol, oc) = self.pos();
        let (op, sign) = match self.advance().tok {
            Tok::PlusEq => (BinOp::Add, "+"),
            Tok::MinusEq => (BinOp::Sub, "-"),
            Tok::StarEq => (BinOp::Mul, "*"),
            _ => (BinOp::Div, "/"),
        };
        if !self.is_known(&name) {
            let e = HelixError::new(format!("`{name}` is not bound, so `{sign}=` has nothing to update"), nl, nc);
            return Err(match self.gone(&name) {
                Some(why) => e.hint(gone_hint(why)),
                None => e.hint(format!(
                    "`{name} {sign}= value` is `{name} = {name} {sign} value`, so `{name}` needs a value first — \
                     bind it with a plain `{name} = …` above this line."
                )),
            });
        }
        let value = self.expr()?;
        let left = Expr::Ident { name: name.clone(), line: nl, col: nc };
        Ok((name, Expr::Binary { op, left: Box::new(left), right: Box::new(value), line: ol, col: oc }))
    }

    /// The fold a loop stands for: `source.reduce(state, (state, item) => do { … })`, where
    /// the state is the carried names — the one name itself, a tuple of several, `()` for
    /// none — and the block reads several out of `$st`, binds a positional pattern from
    /// `$it`, runs the body's lines and answers the state again.
    ///
    /// A source that is not an array is refused in the loop's words rather than `reduce`'s,
    /// by an ordinary `if` around the fold — the only way a message reaches all three
    /// engines without any of them knowing about loops:
    ///
    ///     let $forin = source in
    ///       if type_of($forin) == "Array" then $forin.reduce(…) else raise("`for` needs …")
    ///
    /// The checker says the same sentence when it knows the type ([`is_loop_source`]). A
    /// source that is an array on its face — a literal, a `range(…)` — is folded directly,
    /// which also leaves it where the JIT's native folds look for it.
    #[allow(clippy::too_many_arguments)]
    fn fold_of(
        &mut self,
        at: (usize, usize),
        source_at: (usize, usize),
        number: usize,
        pattern: LoopPattern,
        source: Expr,
        lines: Vec<(String, Expr)>,
        carried: &[(String, usize, usize)],
    ) -> Expr {
        let (l, c) = at;
        let (sl, sc) = source_at;
        let ident = |name: &str| Expr::Ident { name: name.to_string(), line: l, col: c };
        let state = |names: &[(String, usize, usize)]| match names {
            [] => Expr::Tuple(Vec::new()),
            [(one, ..)] => ident(one),
            many => Expr::Tuple(many.iter().map(|(n, ..)| ident(n)).collect()),
        };
        // The lambda's own parameters do the unpacking where they can: one carried name IS
        // the state, so it is the first parameter, and a pattern that is a name is the
        // second. `for x in xs { total += x }` is then `xs.reduce(total, (total, x) => total
        // + x)` to the letter — the fold a person writes, and the one the JIT compiles.
        let state_param = match carried {
            [(one, ..)] => one.clone(),
            _ => "$st".to_string(),
        };
        let item_param = match &pattern {
            LoopPattern::Name(name) if name != "_" => name.clone(),
            _ => "$it".to_string(),
        };
        let mut block: Vec<(String, Expr)> = Vec::with_capacity(carried.len() + lines.len() + 2);
        if carried.len() > 1 {
            for (i, (name, ..)) in carried.iter().enumerate() {
                let read =
                    Expr::Index { recv: Box::new(ident("$st")), index: Box::new(Expr::Int(i as i64)), line: l, col: c };
                block.push((name.clone(), read));
            }
        }
        if let LoopPattern::Positions { names, rest } = pattern {
            // `$it` is a name, so the pattern reads it in place and binds no temp.
            Self::push_positions(&mut block, "$it".to_string(), names, rest, ident("$it"));
        }
        block.extend(lines);
        // A block that ends by binding its one carried name answers that value: `do { …
        // total = e \n total }` is `do { … e }`, and with nothing before it, `e`.
        let mut result = state(carried);
        if let ([(one, ..)], Some((last, _))) = (carried, block.last())
            && last == one
            && let Some((_, value)) = block.pop()
        {
            result = value;
        }
        let body = if block.is_empty() {
            result
        } else {
            Expr::Let { bindings: block, body: Box::new(result), from_do: true }
        };
        let step = Expr::Lambda {
            params: vec![state_param, item_param],
            anns: Vec::new(),
            defaults: Vec::new(),
            bound: None,
            body: std::rc::Rc::new(body),
        };
        let reduce = |recv: Expr| Expr::Method {
            recv: Box::new(recv),
            name: "reduce".to_string(),
            args: vec![state(carried), step],
            named: vec![],
            ufcs: Some(LOOP_FOLD.to_string()),
            line: sl,
            col: sc,
        };
        let an_array_on_its_face = matches!(&source, Expr::Array(_))
            || matches!(&source, Expr::Call { name, .. } if name == "range" && !self.rebound.contains("range"));
        if an_array_on_its_face || self.rebound.contains("type_of") || self.rebound.contains("raise") {
            return reduce(source);
        }
        let held = format!("{LOOP_SOURCE}{number}");
        let at_source = |name: &str| Expr::Ident { name: name.to_string(), line: sl, col: sc };
        let call = |name: &str, args: Vec<Expr>| Expr::Call { name: name.to_string(), args, line: sl, col: sc };
        let is = |left: Expr, right: &str| Expr::Binary {
            op: BinOp::Eq,
            left: Box::new(left),
            right: Box::new(Expr::Str(right.to_string())),
            line: sl,
            col: sc,
        };
        let either = |a: Expr, b: Expr| Expr::Binary { op: BinOp::Or, left: Box::new(a), right: Box::new(b), line: sl, col: sc };
        let when = |cond: Expr, then: Expr, otherwise: Expr| Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then),
            else_branch: Box::new(otherwise),
            line: sl,
            col: sc,
        };
        let kind = "$forkind";
        let starts = |letter: &str| Expr::Method {
            recv: Box::new(at_source(kind)),
            name: "starts_with".to_string(),
            args: vec![Expr::Str(letter.to_string())],
            named: vec![],
            ufcs: None,
            line: sl,
            col: sc,
        };
        // "an Int", "a String", "`missing`" — `value::with_article`, spelled as a value.
        let vowel = either(either(starts("A"), starts("E")), either(starts("I"), starts("O")));
        let named = when(
            is(at_source(kind), "Missing"),
            Expr::Str("`missing`".to_string()),
            when(
                is(at_source(kind), "Node"),
                Expr::Str("a tracked value".to_string()),
                Expr::Interp(vec![
                    InterpPart::Expr(Box::new(when(vowel, Expr::Str("an".to_string()), Expr::Str("a".to_string()))), None),
                    InterpPart::Lit(" ".to_string()),
                    InterpPart::Expr(Box::new(at_source(kind)), None),
                ]),
            ),
        );
        let message = Expr::Interp(vec![InterpPart::Lit(FOR_NEEDS.to_string()), InterpPart::Expr(Box::new(named), None)]);
        let refusal = Expr::Let {
            bindings: vec![(kind.to_string(), call("type_of", vec![at_source(&held)]))],
            body: Box::new(call("raise", vec![message, Expr::Str(FOR_NEEDS_HELP.to_string())])),
            from_do: true,
        };
        let guarded = when(is(call("type_of", vec![at_source(&held)]), "Array"), reduce(at_source(&held)), refusal);
        // (`from_do`, so the lint that advises a `do` block over nested `let`s reads it as one.)
        Expr::Let { bindings: vec![(held, source)], body: Box::new(guarded), from_do: true }
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
