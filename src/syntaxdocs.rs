//! The catalog of LANGUAGE FORMS — syntax, which has no name to look up.
//!
//! **Why this file exists.** `helix search` indexed the API catalog, so every builtin and
//! method was findable and *none of the syntax was*. A field report caught the sharpest
//! version: `helix search raw` returned four rows, all of them `d`**raw**`n at random`,
//! while `"""…"""` — the exact form the report was about — was invisible, because a raw
//! string is not a builtin. `helix search interpolation` returned nothing at all.
//!
//! The same report found `match` this way: a real control-flow form with guards and a `_`
//! default, absent from `AGENTS.md`, invisible to search, and unused across 2,315 lines
//! written by this project's own author — whose router is a twelve-arm `else if` ladder on
//! a String, which is what `match` is for. Nobody was wrong; the two places a writer looks
//! did not mention it.
//!
//! **Every example here is EXECUTED by the gate on all three engines**, exactly like the
//! API catalog's. A syntax note that has rotted is worse than none, because syntax is what
//! a reader trusts without checking.
//!
//! **`notes` carries the vocabulary the form does not contain.** A reader hunting for
//! `match` types "switch" or "case"; one hunting for `missing` types "null" or "none".
//! Searching by intent only works if the words a newcomer brings are somewhere in the
//! corpus, so they are put here deliberately rather than left to chance.

/// One language form: syntax rather than a callable, so it has a `form` where an API entry
/// has a signature.
pub struct SyntaxDoc {
    /// What a reader types after `helix describe`, and the primary search key.
    pub name: &'static str,
    /// The form as written, e.g. `match x { pat => expr, _ => expr }`.
    pub form: &'static str,
    /// One sentence of what it is.
    pub doc: &'static str,
    /// A complete program — run AS WRITTEN, not wrapped in `print`, because most forms
    /// here are statements rather than expressions.
    pub example: &'static str,
    /// Exact stdout of running `example`.
    pub example_out: &'static str,
    /// The surprise worth reading first, plus the words a searcher would arrive with.
    pub notes: &'static str,
}

/// Every language form, in rough order of how often not knowing it has cost someone.
pub static SYNTAX: &[SyntaxDoc] = &[
    SyntaxDoc {
        name: "match",
        form: "match x { literal => expr, name if cond => expr, _ => expr }",
        doc: "Dispatch on a value: literal arms, an optional guard, and `_` for the rest.",
        example: "fn size(n) = match n { 0 => \"none\", x if x > 10 => \"big\", _ => \"small\" }\nprint(size(0), size(20), size(3))",
        example_out: "none big small",
        notes: "The switch/case form. A long `else if` ladder on one value is what this replaces. \
                Arms are separated by commas and `_` is the default; a guard is `name if cond`. \
                Keywords a reader might arrive with: switch, case, pattern, dispatch, cond, ladder.",
    },
    SyntaxDoc {
        name: "raw-string",
        form: "\"\"\"text\"\"\"",
        doc: "A string with NO interpolation and no escapes — every character is literal.",
        example: "print(\"\"\"[0-9]{4} and a \\n stay literal\"\"\")",
        example_out: "[0-9]{4} and a \\n stay literal",
        notes: "THE form for regex patterns, because `{4}` in an ordinary string is interpolation \
                and silently becomes the number 4 (helix check refuses that). Also for Windows \
                paths, JSON templates, and any text with braces or backslashes. \
                Keywords: raw, verbatim, literal, escape, backslash, regex, pattern, template, heredoc, triple quote.",
    },
    SyntaxDoc {
        name: "interpolation",
        form: "\"text {expr} more\"",
        doc: "Embed any expression in a string; `{{` and `}}` are literal braces.",
        example: "n = 3\nprint(\"n={n} sum={n + 1} brace={{lit}}\")",
        example_out: "n=3 sum=4 brace={lit}",
        notes: "Strings have no `+`. This and `join` are the two ways to build one, and both are \
                linear. A `{` you mean literally must be doubled, or use a raw string. \
                Keywords: format, template, concatenate, concat, plus, append, f-string, sprintf, building.",
    },
    SyntaxDoc {
        name: "missing",
        form: "missing",
        doc: "The absent value. It PROPAGATES: any operation on it answers missing.",
        example: "print([1, missing, 3].drop_missing(), missing + 1, missing == missing)",
        example_out: "[1, 3] missing missing",
        notes: "Because `missing == missing` is missing, filtering with `== missing` finds NOTHING \
                silently; the keep-non-missing idiom is `where(@v == @v)` and the explicit form is \
                `drop_missing`. `d.get(k)` answers missing where `d.expect(k)` raises. \
                Keywords: null, none, nil, NA, NaN, absent, undefined, optional, empty, nothing.",
    },
    SyntaxDoc {
        name: "do",
        form: "fn f() = do { stmt\\n stmt\\n result }",
        doc: "A multi-statement body; the last expression is the value. Newlines separate, never `;`.",
        example: "fn f(x) = do {\n  a = x + 1\n  a = a * 2\n  a\n}\nprint(f(3))",
        example_out: "8",
        notes: "`fn` is item-level only — inside a `do` bind a lambda instead. Rebinding a name \
                shadows the previous one, which is how a body evolves state without `mut`. \
                Keywords: block, braces, statements, sequence, multiline, body, begin.",
    },
    SyntaxDoc {
        name: "where",
        form: "fn f(x) = expr where a = ..., b = ...",
        doc: "Bindings written AFTER the expression that uses them.",
        example: "fn hyp(a, b) = root where sq = a * a + b * b, root = sq.sqrt()\nprint(hyp(3.0, 4.0))",
        example_out: "5.0",
        notes: "Lets the answer lead and the scaffolding follow, so a one-line function stays one \
                line. Bindings may refer to earlier ones. \
                Keywords: let, local, helper, binding, temporary, intermediate, subexpression.",
    },
    SyntaxDoc {
        name: "if",
        form: "if cond then a else b",
        doc: "The conditional EXPRESSION — it has a value, and `else` is required.",
        example: "x = 5\nprint(if x > 3 then \"big\" else \"small\")",
        example_out: "big",
        notes: "There is no ternary `?:` and no parenthesized `if (c)`. For dispatch on one value \
                with several outcomes, `match` reads better. \
                Keywords: ternary, conditional, else, elif, branch, question mark.",
    },
    SyntaxDoc {
        name: "fn",
        form: "fn area(w: Int, h: Int) -> Int = w * h",
        doc: "Define a function. Parameter and return annotations are optional, checked by `helix check` before the program runs, and cost nothing at run time.",
        example: "fn area(w: Int, h: Int) -> Int = w * h\nprint(area(3, 4))",
        example_out: "12",
        notes: "Type names: `Int`, `Float`, `Num` (either), `String`, `Bool`, `Array`, `Record`, `Dict`, \
                `Tuple`, `Function`, `DataFrame`, `Tensor`, `Dna`, `Any`. An `Int` annotation refuses a \
                Float argument; a `Float` one accepts an Int. `Record`, `Dict`, `Tuple` and `Function` say \
                what KIND of value arrives without fixing its shape: a wrong kind is refused at the call, \
                and inside the body the value's fields and methods are open. A lambda takes the same \
                annotations: `(x: Int) => x + 1`. A call to a function whose parameter is unannotated, or \
                annotated with an open kind (`Record`, `Dict`, `Tuple`, `Function`, `Array`), re-types its \
                body with what the call passes, so a shape computed from an argument reaches the caller; \
                `x: Any` opts a parameter out of that. A call with literal arguments to one of the \
                program's own functions is evaluated ONCE, before the program runs, and replaced by \
                its value (ADR 0050) — a library's render of a literal spec costs nothing at run time, \
                and a spec the library refuses is refused before the program runs, as a type error is. \
                Anything impure (output, time, a file, the network, a mutable global) is left to run \
                time exactly as written. \
                Keywords: function, define, signature, type, annotation, parameter, return, static, check, typed, constant folding, pure.",
    },
    SyntaxDoc {
        name: "destructure",
        form: "let {where, limit: lim} = spec in expr",
        doc: "Bind fields of a record by name; an absent field is `missing`, and `field: name` binds it under another name.",
        example: "fn build(spec) = let {where, limit: lim} = spec in \"{where} {lim}\"\nprint(build({where: \"x\"}), build({where: \"y\", limit: 3}))",
        example_out: "x missing y 3",
        notes: "The statement form `{where, limit} = spec` works at the top level and inside `do { }` \
                (with `mut` and `export` as for any assignment). The value is evaluated once; each read is \
                `spec.field` that answers `missing` instead of refusing an absent field. Where the checker \
                knows the record's shape, a name it cannot have is refused. Rename a field when the name \
                is taken — a module that defines `select` destructures a spec's `select` as `{select: sel}`. \
                Keywords: pattern, unpack, fields, spec, options record, rename, alias.",
    },
    SyntaxDoc {
        name: "lambda",
        form: "(x) => expr",
        doc: "An anonymous function value, bindable to a name or passed to a method.",
        example: "double = (x) => x * 2\nprint([1, 2, 3].map(double(it)), [1,2,3].reduce(0, (acc, x) => acc + x))",
        example_out: "[2, 4, 6] 6",
        notes: "The form to use inside `do { }`, where `fn` is not allowed. A function stored in a \
                record field is called as a method: `rec.f(x)`. Parameters take the annotations a `fn` \
                does: `(x: Int, y: Int) => x + y` — and they hold wherever the lambda is called from: by \
                name, as a record field `rec.f(x)`, or as a value `(rec.f)(x)`, `helix check` refuses a \
                wrong argument or count the same way. \
                Keywords: closure, anonymous, arrow, callback, function value, higher order.",
    },
    SyntaxDoc {
        name: "spread",
        form: "{...base, field: value, ...more}",
        doc: "A record built from other records: each `...spread` contributes a record's (or a dict's) fields, each named field one value, and a later part wins.",
        example: "base = {name: \"Ada\", age: 41}\nprint({...base, age: 42}, {...base, ...{city: \"oslo\", age: 43}})",
        example_out: "{age: 42, name: \"Ada\"} {age: 43, city: \"oslo\", name: \"Ada\"}",
        notes: "The one way to derive a record from an immutable one, and the one way to MERGE two: \
                `{...ADULTS, ...NEWEST}` combines two reusable query fragments, later fields winning, \
                exactly as `{...base, field: value}` lets a named field win. The spread comes first; \
                any number of spreads and fields may follow, in written order. A dict spreads as its \
                string keys. The checker follows a spread of a known record, so a typo after a merge is \
                refused; a spread whose shape it cannot see (a parameter, parsed JSON, a dict) makes \
                the result an open record. A field named twice in one literal is refused. \
                Keywords: spread, merge, update, copy with, combine records, object spread, scope.",
    },
    SyntaxDoc {
        name: "it",
        form: "xs.map(it * 2)",
        doc: "The current element inside a comprehension — no parameter to name.",
        example: "print([1, 2, 3].map(it * 2).where(it > 2).sum())",
        example_out: "10",
        notes: "These chains are the loop: Helix has no `for`. A numeric chain over packed arrays is \
                also what the JIT compiles — `helix jit-explain` says whether yours was. \
                Keywords: loop, for, each, iterate, element, current, implicit, placeholder, underscore.",
    },
    SyntaxDoc {
        name: "column",
        form: "df.where(@name > 1)",
        doc: "`@name` refers to a DataFrame column inside a frame verb.",
        example: "print(dataframe({a: [1, 2, 3]}).where(@a > 1).count())",
        example_out: "2",
        notes: "A bare `name` would be an ordinary binding, so the `@` is what makes a column \
                reference visible at the call site. \
                A column takes the String tests too — starts_with, ends_with, contains and re_match — so a text filter is a query rather than a round trip through JSON. Keywords: dataframe, column, field, select, filter, expression, reference, table, regex, text, string.",
    },
    SyntaxDoc {
        name: "try",
        form: "try (expr)",
        doc: "Run an expression that may raise, answering {ok, value, error} instead.",
        example: "r = try (1 % 0)\nprint(r.ok, r.error)",
        example_out: "false modulo by zero",
        notes: "It binds TIGHTER than operators, so write `try (a + b)` — never `try a + b`. Do not \
                use it as a type test: it is far more expensive than `type_of`. \
                Keywords: error, exception, catch, rescue, result, fallible, handle, recover, panic.",
    },
    SyntaxDoc {
        name: "mut",
        form: "mut n = 0",
        doc: "A rebindable top-level binding — the only mutable state in the language.",
        example: "mut n = 0\nn = n + 1\nprint(n)",
        example_out: "1",
        notes: "TOP-LEVEL ONLY, and that is a design question rather than a spelling one: a function \
                body evolves state by rebinding inside `do { }`, and state crossing a sequence is \
                threaded with `reduce`. Reach for `mut` only for state that must outlive a call. \
                Keywords: mutable, variable, assign, reassign, update, counter, accumulator, global, state.",
    },
    SyntaxDoc {
        name: "import",
        form: "import lib.stats as st",
        doc: "Bring in another module: whole, aliased, specific names, or every export.",
        example: "print(1)",
        example_out: "1",
        notes: "`import lib.stats` for `lib/stats.helix`, `as st` to alias, `import lib.stats.{mean, sd}` \
                to bring names in unqualified, or `import lib.stats.*` for every export — \
                `import lib.stats.* except {mean}` declines some. A glob name that is a builtin is \
                refused unless the file also imports it by name or declines it; an `except` name must \
                be an export. Not `use`, not `from … import …`. \
                Keywords: module, use, require, include, package, library, namespace, dependency, glob, star, wildcard.",
    },
    SyntaxDoc {
        name: "main",
        form: "fn main(seq: String, threads: Int = 1) = ...",
        doc: "If a program defines `fn main`, its parameters ARE the command line.",
        example: "print(1)",
        example_out: "1",
        notes: "Arguments bind by the ordinary call-site rule — positional, `--named value`, `--named=value`, \
                out of order — and a Bool parameter is a bare flag. A doc comment above `main` becomes \
                `--help`. \
                Keywords: command line, argv, cli, arguments, flags, options, parse args, entry point, script, tool.",
    },
];

/// The form named `name`, if it is one.
pub fn syntax_doc(name: &str) -> Option<&'static SyntaxDoc> {
    SYNTAX.iter().find(|s| s.name == name)
}
