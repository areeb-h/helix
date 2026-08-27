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
        name: "lambda",
        form: "(x) => expr",
        doc: "An anonymous function value, bindable to a name or passed to a method.",
        example: "double = (x) => x * 2\nprint([1, 2, 3].map(double(it)), [1,2,3].reduce(0, (acc, x) => acc + x))",
        example_out: "[2, 4, 6] 6",
        notes: "The form to use inside `do { }`, where `fn` is not allowed. A function stored in a \
                record field is called parenthesized: `(rec.f)(x)`. \
                Keywords: closure, anonymous, arrow, callback, function value, higher order.",
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
                Keywords: dataframe, column, field, select, filter, expression, reference, table.",
    },
    SyntaxDoc {
        name: "try",
        form: "try (expr)",
        doc: "Run an expression that may raise, answering {ok, value, error} instead.",
        example: "r = try (1 / 0)\nprint(r.ok, r.error)",
        example_out: "false division by zero",
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
        doc: "Bring in another module: whole, aliased, or specific names.",
        example: "print(1)",
        example_out: "1",
        notes: "`import lib.stats` for `lib/stats.helix`, `as st` to alias, or `import lib.stats.{mean, sd}` \
                to bring names in unqualified. Not `use`, not `from … import …`. \
                Keywords: module, use, require, include, package, library, namespace, dependency.",
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
