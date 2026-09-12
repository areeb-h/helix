# ADR 0052 — A column expression is a value

- **Status:** **Accepted & implemented 2026-09-08** (0.10.0-to-be). The user's decision:
  the field build's ORM was spelling a condition as an operator inside a string key
  (`{"age >": 30}`) or as a triple (`[["age", ">", 30]]`), and asked for better.
- **Decision:** `@age > 30` — the way Helix already spells a condition inside a frame verb
  — is a value everywhere: the record that describes it, which a library reads like any
  record, which the load-time specializer (ADR 0051) sees through, and which a frame verb
  accepts back through a name. The argument list of a frame verb keeps the frame reading.

## Context

Inside a frame verb a column expression is unevaluated: `df.where(@age > 30 and
@city == c)` hands the frame an expression over its columns, compiled to the engine's
`ColExpr`. Outside one it was a check-time error ("only valid inside a DataFrame
operation"). So a library that wanted a condition as data — an ORM rendering `where age >
$1` — had to invent a spelling of its own. The field build chose a quoted key carrying the
operator, `{where: {"age >": 30}}`, and a quoted key builds a dict, which the specializer
cannot see through (ADR 0051's `any_of` case never reached its clause text); the
alternative it found, `[["age", ">", 30]]`, the specializer does see through and nobody
wants to write. Both are the language's condition syntax rebuilt by hand, worse.

## The design

**One spelling.** A condition is a column expression: `@age > lo and @city == c`,
`not @name.starts_with("x")`, `@v.is_missing()`, `is_nan(@f)`, `-@a + @b`. Inside a
frame verb's argument list it is the frame's, as before. Anywhere else it evaluates to a
**predicate record**, node for node the frame engine's own grammar:

| expression                                   | value                                                           |
|----------------------------------------------|-----------------------------------------------------------------|
| `@name`                                      | `{kind: "col", name: "name"}`                                   |
| a literal, a name, a call — anything else    | `{kind: "lit", value: v}`, evaluated where it stands            |
| `a > b`, `a + b`, `a and b`, … (any operator) | `{kind: "bin", op: ">", left: a, right: b}`, the operator's spelling |
| `not a`, `-a`                                | `{kind: "not", expr: a}`, `{kind: "neg", expr: a}`              |
| `a.is_missing()`                             | `{kind: "is_missing", expr: a}`                                 |
| `a.is_nan()`, `is_finite(a)`                 | `{kind: "is_nan", expr: a}`, `{kind: "is_finite", expr: a}`     |
| `a.starts_with("x")`, `ends_with`, `contains`, `re_match` | `{kind: "str", name: "starts_with", expr: a, args: ["x"]}` |

The value's fields are the record's: `p.kind`, `p.op`, `p.left.name`, `p.right.value`. It
prints, compares, serializes and destructures as a record does. Nothing is assumed about
the values: `{kind: "lit", value: lo}` holds whatever `lo` is at that moment.

**Back into a frame.** A frame verb handed a NAME bound to a predicate — `p = @age > 30;
df.where(p)`, or a predicate a library built — reads it as the expression it describes,
with the frame's own checks: a column must exist, a literal must be a scalar. A record that
is not a predicate is refused as any non-scalar variable was. So a condition can be built
in one place, kept, combined (`p and @x > 1` is a column expression over a name, and a
value), rendered by an ORM, and run on a frame — one thing.

**Where the rewrite lives.** The parser's last pass over the finished tree: a column
expression anywhere but in the argument list of a method named like a frame verb
(`where`, `filter`, `select`, `sort`, `group`, `with`, `join`, `count`, `agg`, …) becomes
its record. It is a syntactic rule, decided by position, so the checker types the record,
the fold sees a record literal, and every engine evaluates a record literal — the engines
cannot disagree. A record's method of a frame verb's name receives a predicate through a
binding; the argument position is the frame's, and the check-time message says so.

**What the specializer sees.** `sql({where: @age > lo and @city == c})` hands `sql` a
shape whose `kind`, `op`, `left.name` and `right.kind` are literals and whose values are
`Any`. A renderer walking it — `if p.kind == "bin" and p.op == "and" then … else "{p.left.name}
{p.op} ${n}"` — reduces to its text with the values as the parameters, and a helper whose
clone becomes a record or array literal with nothing to run in its leaves is inlined at
its call site with the arguments in for the parameters, so a recursive renderer's
`{s, n, ps}` results compose at load time. The clause text is a constant; only `lo` and
`c` are work.

## Consequences

- The field build's `any_of` and `where` can be spelled `{where: @city == c and (@age > lo
  or @vip == true)}` and rendered as a constant; the quoted-key dict form stays valid and
  stays opaque to the specializer.
- A program that relied on `@age > 30` outside a frame verb being a check-time error no
  longer sees one: the expression is a record. No shipped program did, since it could not
  run.
- The inlining of a clone that becomes a record or array literal reaches the field build's
  existing spellings — their clause builders return `{s, n, ps}` records. Their harness,
  the previous commit's binary against this one, both built fresh, interleaved, min of
  three runs of its median of 5 trials of 2 000: `where eq` 2.039 → 0.830 µs, `delete`
  1.857 → 0.780 µs, `update` 1.842 → 1.693 µs, `OR two branches` 6.129 → 5.775 µs,
  `prepared bind only` 0.252 → 0.230 µs; the rest within the noise; statements identical;
  `helix check` of the harness 19.7 → 20.2 ms.
- Pinned by the `predicate` module's tests (every node's record; a frame verb's argument
  left alone; a value read back as the frame's expression), the fold's
  `a_predicate_at_a_call_site_renders_to_its_text`, and
  `a_column_expression_is_a_value_on_every_engine` (three engines, each pass off), plus the
  corpus program `predicates_are_values.helix`.
