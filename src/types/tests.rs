    use super::*;

    fn tc(src: &str) -> Result<(), HelixError> {
        let toks = crate::lexer::lex(src)?;
        let prog = crate::parser::parse(toks)?;
        check(&prog).map(|_| ())
    }
    fn ok(src: &str) {
        let r = tc(src);
        assert!(r.is_ok(), "expected OK for `{}`, got: {:?}", src, r.err().map(|e| e.message));
    }
    fn emsg(src: &str) -> String {
        tc(src).expect_err("expected a type error").message
    }

    #[test]
    fn well_typed_programs_pass() {
        ok("x = 5\ny = x + 1");
        ok("[1, 2, 3].map(it + 1).sum()");
        ok("grade = if 5 > 3 then \"A\" else \"B\"");
        ok("missing + 1");
        ok("[1, missing, 3].mean()");
        ok("x = 5\nx.is_missing()");
        ok("seq = dna(\"ATGC\")\nseq.gc_content()");
        ok("tensor([[1, 2], [3, 4]]).matmul(tensor([[1, 0], [1, 1]])).sum()");
        ok("tensor([3, 4]).norm()");
        ok("scores = [1, 2, 3]\nscores.reduce(0, (a, x) => a + x)");
        ok("grid = [[1, 2], [3, 4]]\ngrid.map(row => row.map(v => v + 1))");
        // recursion (un-annotated params -> Unknown, never false-positive)
        ok("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(5)");
        ok("[1, \"a\", true].count()"); // mixed array -> Array(Unknown), fine
    }

    #[test]
    fn annotations_check() {
        ok("fn area(w: Int, h: Int) -> Int = w * h\narea(3, 4)");
        ok("fn f(x: Int) = x + 1\nf(2)");
        ok("fn norm2(xs) -> Float = sqrt((xs * xs).sum())"); // body Unknown compat any ret
    }

    #[test]
    fn dataframe_columns_unchecked() {
        // column names are the runtime schema boundary — never type-checked
        ok("read_csv(\"x.csv\").where(age > 40 and hr < 75).select(name, age).sort(age).count()");
        ok("read_csv(\"g.csv\").group(species).mean(expression).columns()");
        // `with` derives columns; `join` combines frames — both keep their args (column
        // names, the other frame, the join type) at the unchecked runtime boundary.
        ok("read_csv(\"p.csv\").with({adult: age >= 18}).select(name, adult).count()");
        ok("read_csv(\"a.csv\").join(read_csv(\"b.csv\"), id, \"left\").sort(id).count()");
        // `column` bridges a frame to an array, so the array statistics chain off it.
        ok("read_csv(\"p.csv\").column(\"age\").median()");
        ok("correlation(read_csv(\"p.csv\").column(\"a\"), read_csv(\"p.csv\").column(\"b\"))");
    }

    #[test]
    fn statistics_typecheck() {
        // Descriptive stats on a numeric array yield Float; `quantile` takes one arg.
        ok("[1, 2, 3].median() + [1, 2, 3].var() + [1.0].quantile(0.5)");
        // `summary` is a record; its fields are reachable and numeric.
        ok("s = [1, 2, 3].summary()\ns.mean + s.std + s.median");
        // `correlation` is a Float-valued function of two arrays.
        ok("sqrt(correlation([1, 2, 3], [3, 2, 1]) * 1.0)");
        // Inferential: `t_test` is a record; the normal functions broadcast like math.
        ok("r = t_test([1.0, 2.0, 3.0], [2.0, 3.0, 4.0])\nr.statistic + r.df + r.p_value");
        ok("normal_cdf(1.96) + erf(1.0) + normal_pdf(0.0)");
        // `linear_regression` is a record; predictions broadcast the fitted line.
        ok("f = linear_regression([1.0, 2.0, 3.0], [2.0, 4.0, 5.0])\nf.slope * 6.0 + f.intercept");
        // `multiple_regression` returns a record whose coefficients are an array.
        ok("m = multiple_regression([[1.0, 2.0, 3.0]], [2.0, 4.0, 5.0])\nm.coefficients[0] + m.r_squared");
    }

    #[test]
    fn error_hints_guide_common_mistakes() {
        // `+` to join strings → point at interpolation / join (compile-time path).
        let h = tc("\"a\" + \"b\"").expect_err("expected error").hint.unwrap_or_default();
        assert!(h.contains("interpolation") || h.contains("join"), "got: {h:?}");
        // `=` (assignment) where `==` (equality) was meant, in a condition (parse path).
        let h2 = tc("if x = 5 then 1 else 2").expect_err("expected error").hint.unwrap_or_default();
        assert!(h2.contains("=="), "got: {h2:?}");
    }

    #[test]
    fn catches_provable_errors() {
        assert!(emsg("5 + \"x\"").contains("needs numbers"));
        assert!(emsg("if 5 then 1 else 2").contains("must be a boolean"));
        assert!(emsg("velociti(3)").contains("not a known function"));
        assert!(emsg("[1, 2].maen()").contains("no method"));
        assert!(emsg("5 and true").contains("boolean"));
        assert!(emsg("range(1, 2, 3, 4)").contains("1 to 3"));
        assert!(emsg("fn f(x: Int) -> String = x + 1").contains("declared to return"));
        assert!(emsg("xs = [1, 2]\nxs[\"a\"]").contains("cannot be indexed by a string"));
        // A known Record indexed by a *dynamic* key is runtime field access — must be
        // accepted (it runs; rejecting it broke the never-reject-runnable promise).
        ok("CODE = {a: 1, b: 2}\nfn f(k) = CODE[k]\nf(\"a\")");
        ok("CODE = {a: 1}\nks = [\"a\"]\nks.map(k => CODE[k])");
        assert!(emsg("undefinedvar").contains("not defined"));
        assert!(emsg("dna(5)").contains("expected a string"));
        // bitwise operators are int-only and statically checked
        assert!(emsg("\"a\" & 1").contains("bitwise"));
        ok("x = 6\n(x >> 1) & 1");
        // namespaced builtins are type-checked: wrong argument types and arities are caught
        assert!(emsg("correlation(1, 2)").contains("array"));
        assert!(emsg("read_vcf(5)").contains("string"));
        assert!(emsg("sqrt(1, 2)").contains("argument"));
        // a bare removed-namespace name reads as plain "not defined" (it may be a typo
        // or an un-imported module), with the namespace history + import path in the hint
        assert!(emsg("x = stats").contains("not defined"));
        let stats_err = tc("x = stats").expect_err("expected a type error");
        let hint = stats_err.hint.unwrap_or_default();
        assert!(hint.contains("namespace") && hint.contains("import stats"));
        // an old namespaced call points at the new spelling (function or method)
        assert!(emsg("stats.t_test([1.0], [2.0])").contains("no longer available"));
        assert!(emsg("json.parse(\"[]\")").contains("no longer available"));
        // method typos suggest the right method; a wrong receiver type is rejected
        assert!(emsg("\"abc\".gc_content()").contains("no method"));
        assert!(emsg("xs = [1, 2]\nxs.summary().nope").contains("field"));
    }

    #[test]
    fn let_in_typecheck() {
        ok("let x = 5 in x + 1");
        ok("let a = 1, b = a + 1 in a + b"); // sequential
        ok("fn variance(xs) = let m = xs.mean(), n = xs.count() in xs.map((it - m) ** 2).sum() / n");
        // a type error inside the body is caught
        assert!(emsg("let x = \"a\" in x + 1").contains("needs numbers"));
        // the let binding's scope doesn't leak: `y` is undefined outside
        assert!(emsg("z = let y = 1 in y\ny").contains("not defined"));
    }

    #[test]
    fn tuples_and_destructuring_typecheck() {
        ok("p = (3, 4)\np[0] + p[1]"); // homogeneous tuple index -> Int
        ok("a, b = (1, 2)\na + b");
        ok("x, y, z = [1, 2, 3]\nx + y + z"); // array destructure
        ok("fn pair(n) = (n, n + 1)\nlo, hi = pair(5)\nlo + hi");
        ok("[1, 2].zip([3, 4]).map(it[0] + it[1])");
        ok("[7, 8].enumerate().map(it[0])");
        // lambda-param destructuring (the nicer form)
        ok("[(1, 2), (3, 4)].map((a, b) => a + b)");
        ok("[1, 2].zip([3, 4]).map((a, b) => a + b)");
        ok("[7, 8].enumerate().where((i, v) => v > 0).map((i, v) => i)");
        // length mismatch is caught at compile time (tuple has a known arity)
        assert!(emsg("a, b = (1, 2, 3)").contains("cannot destructure"));
        // destructuring a scalar is a compile error
        assert!(emsg("a, b = 5").contains("cannot destructure"));
    }

    #[test]
    fn slicing_typecheck() {
        ok("xs = [1, 2, 3, 4]\nxs[1:3].sum()"); // slice of array stays an array
        ok("\"hello\"[::-1].upper()"); // slice of string stays a string
        ok("xs = [1, 2, 3]\nxs[:]"); // bare slice
        // a non-integer bound is a compile error
        assert!(emsg("xs = [1, 2, 3]\nxs[\"a\":]").contains("integer"));
        // slicing a non-sliceable type errors
        assert!(emsg("(5)[1:2]").contains("cannot be sliced"));
    }

    #[test]
    fn records_typecheck() {
        ok("r = {name: \"Ada\", age: 41}\nr.age + 1");
        ok("fn stats(xs) = {mean: xs.mean(), n: xs.count()}\nstats([1, 2, 3]).mean");
        ok("[{age: 10}, {age: 20}].map(it.age).mean()");
        // field typo caught at compile time, with a suggestion
        assert!(emsg("r = {name: \"A\", age: 1}\nr.naem").contains("no field"));
        assert_eq!(
            tc("r = {name: \"A\"}\nr.naem").unwrap_err().hint.as_deref(),
            Some("did you mean `name`?")
        );
        // method without parens → helpful "call it with ()" hint
        assert!(tc("[1, 2].mean")
            .unwrap_err()
            .hint
            .as_deref()
            .unwrap()
            .contains("call it with `mean()`"));
    }

    #[test]
    fn interpolation_and_coalesce() {
        ok("name = \"x\"\nprint(\"hi {name} {1 + 2}\")");
        ok("x = missing\nprint(\"v = {x ?? 0}\")");
        ok("config = missing\ntimeout = config ?? 30");
        // embedded expressions are type-checked: undefined names error
        assert!(emsg("print(\"hi {nope}\")").contains("not defined"));
        // ?? never errors (any operands)
        ok("\"a\" ?? 1");
    }

    #[test]
    fn suggests_on_typos() {
        assert_eq!(
            tc("[1, 2].maen()").unwrap_err().hint.as_deref(),
            Some("did you mean `mean`?")
        );
    }

    #[test]
    fn unknown_type_annotation_errors() {
        assert!(emsg("fn g(x: Intt) = x").contains("unknown type"));
    }

    #[test]
    fn examples_have_zero_false_positives() {
        // THE hard guarantee: every shipped example must type-check clean.
        for name in [
            "language/tour",
            "language/functions",
            "language/control-flow",
            "language/operators",
            "language/interpolation",
            "language/collections",
            "language/errors",
            "language/typed",
            "language/strings",
            "language/records",
            "language/slicing",
            "language/tuples",
            "language/bindings",
            "numerics/math",
            "numerics/vectors",
            "numerics/lattice",
            "numerics/tensors",
            "dataframes/analysis",
            "dataframes/dataframes",
            "dataframes/io",
            "statistics/metrics",
            "statistics/regression",
            "bio/genomics",
        ] {
            let src = std::fs::read_to_string(format!("examples/{}.helix", name))
                .unwrap_or_else(|_| panic!("read examples/{}.helix", name));
            let toks = crate::lexer::lex(&src).expect("lex");
            let prog = crate::parser::parse(toks).expect("parse");
            let r = check(&prog);
            assert!(
                r.is_ok(),
                "example `{}` must type-check clean, got: {:?}",
                name,
                r.err().map(|e| e.message)
            );
        }
    }

/// Every builtin answers the signature probe (docs/dx-plan.md, describe enrichment).
///
/// `helix describe` derives each builtin's arity by probing [`super::probe_builtin`]
/// with `Unknown` argument vectors: the accepted lengths are the signature, and a
/// builtin whose checker arm never looks at `args.len()` accepts every probe and is
/// reported `signatures: null` — honest, not fabricated. What must never happen is the
/// THIRD state: a builtin rejecting every probe, which would mean its checker arm
/// rejects `Unknown` arguments — and since `compatible(Unknown, _)` is the permissive
/// checker's foundation, that is a checker bug before it is a catalog bug. This pin
/// turns a future Unknown-rejecting guard into a gate failure instead of a silently
/// signature-less catalog entry.
#[test]
fn every_builtin_answers_the_signature_probe() {
    for b in crate::registry::BUILTINS {
        let accepted: Vec<usize> = (0..=8)
            .filter(|&k| super::probe_builtin(b.path, &vec![super::Type::Unknown; k]).is_some())
            .collect();
        assert!(
            !accepted.is_empty(),
            "`{}` rejected every arity probe 0..=8 — its checker arm rejects Unknown \
             arguments (a permissive-checker violation), or its arity exceeds the probe \
             range",
            b.path
        );
    }
}

    /// The annotation names a library actually needs — `Record`, `Dict`, `Tuple`, `Function`,
    /// `Any` — and what they mean: a wrong KIND of argument is refused at the call; the value
    /// stays open inside the body (field build, 1.45a).
    #[test]
    fn annotations_name_every_value_kind() {
        ok("fn f(r: Record) = r.name\nf({name: \"x\"})");
        ok("fn f(r: Record) = r.get(\"k\")\nf({k: 1})");
        ok("fn f(r: Record) = {...r, z: 1}\nf({k: 1})");
        ok("fn g(d: Dict) = d.get(\"k\")\ng([[\"k\", 1]].to_dict())");
        ok("fn h(t: Tuple) = t[0]\nh((1, 2))");
        ok("fn k(f: Function, x) = f(x)\nk((v) => v + 1, 1)");
        ok("fn a(x: Any) = x\na(1)\na(\"s\")");
        assert!(emsg("fn f(r: Record) = r\nf(1)").contains("should be Record"));
        assert!(emsg("fn g(d: Dict) = d\ng({a: 1})").contains("should be Dict"));
        assert!(emsg("fn h(t: Tuple) = t\nh([1, 2])").contains("should be Tuple"));
        assert!(emsg("fn k(f: Function) = f\nk(3)").contains("should be Function"));
        assert!(emsg("fn f(x: Rekord) = x").contains("unknown type"));
    }

    /// `(x: Int) => x` — a lambda's parameters take the annotations a `fn` does (1.45b), and
    /// an `Int` annotation refuses a Float where the numeric tower used to wave it through
    /// (1.45c): `type_of(1.5)` is `"Float"`, so static and dynamic agree. `Float` still admits
    /// an Int, and `Num` both.
    #[test]
    fn lambda_annotations_and_int_means_int() {
        ok("f = (x: Int, y: Int) => x + y\nf(1, 2)");
        assert!(emsg("f = (x: Int) => x\nf(\"s\")").contains("should be Int"));
        assert!(emsg("fn f(x: Int) = x\nf(1.5)").contains("should be Int"));
        ok("fn f(x: Float) = x\nf(1)");
        ok("fn f(x: Num) = x\nf(1)\nf(1.5)");
        assert!(emsg("fn f() -> Int = 1.5").contains("declared to return Int"));
        ok("fn f() -> Float = 1");
    }

    /// An argument-dependent record shape crosses a CALL (field build, 1.44): the checker
    /// re-types an unannotated function's body with the call site's argument types and uses
    /// the answer only when it is more precise. A body that does not type under the
    /// specialization keeps the definition's permissive answer, so nothing that ran is
    /// refused; recursion terminates on the in-progress guard.
    #[test]
    fn a_call_specializes_an_unannotated_function_on_its_arguments() {
        assert!(emsg("fn mk(s) = {c: s.columns}\nmk({columns: {id: 1, name: 2}}).c.nmae").contains("no field `nmae`"));
        assert!(emsg("fn mk(s) = let m = {c: s.columns} in {...m, f: (x) => x}\nmk({columns: {id: 1, name: 2}}).c.nmae").contains("no field"));
        assert!(emsg("fn id(s) = s\nid({id: 1}).nmae").contains("no field `nmae`"));
        assert!(emsg("fn mk(s) = {c: s.columns}\nu = mk({columns: {id: 1, name: 2}})\nu.c.nmae").contains("no field `nmae`"));
        ok("fn mk(s) = {c: s.columns}\nmk({columns: {id: 1, name: 2}}).c.name");
        // A parameter the call leaves Unknown stays permissive.
        ok("fn mk(s) = {c: s.columns}\nfn wrap(t) = mk(t).c.nmae\nwrap(1)");
        // A body the specialization cannot type keeps the definition's answer: no refusal.
        ok("fn pick(x) = if type_of(x) == \"Int\" then x + 1 else x.name\npick(1)\npick({name: \"a\"})");
        // Recursion terminates and still answers.
        ok("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(5) + 1");
        // Through a function-valued argument, the callee's own call types precisely.
        ok("fn apply(f, x) = f(x)\napply((v) => v + 1, 1)");
        // A body that fails only under the call's types is NOT surfaced — the definition's
        // answer stands (the field might sit in a branch these arguments never take) …
        ok("fn f(x: Int, r) = r.name\nf(1, {nmae: 2})");
        // … while the annotated parameter keeps its annotation and the other specializes,
        // so a shape refused at the CALLER is refused.
        assert!(emsg("fn f(x: Int, r) = r\nf(1, {nmae: 2}).name").contains("no field `name`"));
        // A local lambda shadowing a top-level fn of the same name is NOT that fn's body.
        ok("fn g(x) = match x + 1 { x => (z => x + z) }\nfn m() = let g = (u => u + 1) in g(0) * 10\nm()");
        // `Any` keeps a function opaque on purpose: the laundering stays a laundering.
        ok("fn launder(x: Any) = x\nlaunder(true) < launder(false)");
        assert!(emsg("fn launder(x) = x\nlaunder(true) < launder(false)").contains("cannot order a Bool"));
        // A destructure of a constructor's (now known) shape answers `missing` for an absent
        // field, as the form promises; a literal written right there is still refused for a
        // name it lacks.
        ok("fn mk(s) = {where: s}\n{where, order} = mk(1)\norder");
        ok("fn mk(s) = {where: s}\nlet {where, order} = mk(1) in order");
        assert!(emsg("{limt} = {limit: 1}").contains("no field `limt`"));
        assert!(emsg("let {limt} = {limit: 1} in limt").contains("no field `limt`"));
    }
