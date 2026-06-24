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
    }

    #[test]
    fn catches_provable_errors() {
        assert!(emsg("5 + \"x\"").contains("needs numbers"));
        assert!(emsg("if 5 then 1 else 2").contains("must be a boolean"));
        assert!(emsg("velociti(3)").contains("not a known function"));
        assert!(emsg("[1, 2].maen()").contains("no method"));
        assert!(emsg("5 and true").contains("boolean"));
        assert!(emsg("range(1, 2, 3)").contains("1 or 2"));
        assert!(emsg("fn f(x: Int) -> String = x + 1").contains("declared to return"));
        assert!(emsg("xs = [1, 2]\nxs[\"a\"]").contains("cannot be indexed by a string"));
        assert!(emsg("undefinedvar").contains("not defined"));
        assert!(emsg("dna(5)").contains("expected a string"));
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
            "tour",
            "functions",
            "analysis",
            "math",
            "dataframes",
            "tensors",
            "errors",
            "typed",
            "strings",
            "records",
            "slicing",
            "tuples",
            "bindings",
            "genomics",
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
