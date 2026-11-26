# General-code results

Instructions executed (callgrind `Ir`) by `bench/general/run.sh`, on a `--profile gate`
build, x86-64 Linux. "Suite" is the 56 programs under `examples/` that run without a
fixture (language, numerics, dataframes, statistics, bio, modules, rules), each run once.

## Where the time went before any change

Measured on the branch point (`9076065`). `ops` is bytecode instructions executed, counted
with a temporary counter in the dispatch loop (never committed).

| workload | Ir | ops | Ir/op | in the VM | dispatch loop alone |
|---|--:|--:|--:|--:|--:|
| records | 915.9 M | 3.99 M | 229 | 99% | 28% |
| nested | 966.3 M | 4.15 M | 232 | 99% | 28% |
| strings | 438.4 M | 1.30 M | 335 | 99% | 19% |
| closures | 839.9 M | 4.40 M | 190 | 98% | 34% |
| match | 1207.5 M | 6.16 M | 196 | 100% | 33% |
| dicts | 585.4 M | 1.77 M | 330 | 100% | 20% |
| methods | 1037.0 M | 3.32 M | 311 | 100% | 21% |
| calls | 1098.0 M | 5.52 M | 198 | 99% | 33% |
| app | 796.8 M | 0.32 M | 2458 | 34% | 3% |
| gallery | 2153.5 M | 3.17 M | 679 | 63% | 10% |
| orbit | 904.3 M | 0.81 M | 1110 | 24% | 6% |
| freezer_model | 1046.0 M | 4.84 M | 215 | 97% | 30% |
| updates_model | 1408.7 M | 6.74 M | 208 | 98% | 31% |
| lockout_model | 613.2 M | 2.58 M | 237 | 95% | 27% |

"In the VM" is everything under `vm::run`, the Rust it calls included. "Dispatch loop
alone" is `vm::run`'s own instructions: nothing but fetching the next instruction and
calling the code for it.

Three findings, in the order they mattered.

**1. A third of everything was the dispatch loop calling a closure.** The loop ran each
instruction inside a closure, so that an error could be caught and routed to a `try`; a
comment said LLVM inlines it. It does not — the closure's body is every opcode — so each
instruction paid for a real call. The disassembly of the loop shows it: 31 instructions
writing the closure's 18 captured references to the stack, then the call, then the callee
saving and restoring registers. `vm::run`'s own cost divided by ops executed is 65
instructions per op on every VM-bound workload, whatever the op did.

**2. What the ops are.** Share of ops executed, the largest first:

| workload | ops |
|---|---|
| records | LoadLocal 24%, GetField 12%, Jump 10%, CompNext 9%, LoadLocalConstBinary 8%, ConstBinary 6%, CompMapPush 6%, CallFn 5%, Return 5%, UpdateRecord 3% |
| nested | GetField 24%, LoadLocal 21%, Const 8%, Binary 7%, Index 5%, then Jump, CompNext, CallFn, Return, UpdateRecord, MakeRecord at 4% each |
| strings | Jump 15%, CompNext 14%, LoadLocal 14%, CompMapPush 11%, Const 10%, Method 8%, LoadLocalConstBinary 8%, Index 6%, Interp 4% |
| closures | LoadLocal 23%, Return 9%, Jump 9%, CompNext 9%, Binary 7%, CallValue 7%, CompMapPush 7%, GetUpvalue 7%, GetField 6% |
| match | LoadLocal 20%, MatchArm 12%, JumpIfFalse 12%, Jump 10%, StoreLocal 8%, LoadLocalConstBinary 7%, CompNext 6%, Const 6% |
| dicts | LoadLocal 16%, CompNext 14%, Jump 14%, Method 10%, LoadLocalConstBinary 7%, ConstBinary 7%, LoadGlobal 7%, InsertIntoLocal 5% |
| methods | Method 19%, LoadLocal 14%, CompNext 12%, Jump 12%, Const 12%, CompMapPush 9% |
| calls | LoadLocal 30%, GetField 10%, Binary 9%, CallFn 7%, Return 7%, JumpIfFalse 6%, Jump 5% |
| freezer_model | LoadLocal 26%, Jump 9%, JumpIfFalse 9%, StoreLocal 8%, ReceiverIs 6%, GetField 5%, CompNext 5%, Method 5% |
| updates_model | LoadLocal 18%, JumpIfFalse 9%, LoadLocalConstBinary 9%, GetField 9%, Jump 7%, AndCheck 7%, Const 7%, StoreLocal 6% |

The commonest pair everywhere records are read is `LoadLocal; GetField` — 9–15% of all
ops on records, nested, calls and the rule tests. Field LOOKUP itself (`eval_field`, the
linear search) is 1–4% of instructions: records here have two to five fields, and finding
one of five by comparing `u32`s is cheap. What a field read costs is the dispatch of two
ops and the clone and drop of the record around the search, not the search.

After the dispatch loop, the costs are spread thin: `Value::clone` 3–5%, dropping a `Value`
3–5%, the allocator 4–8% (a record is three allocations: the argument vector, the field
vector, the `Rc`), moving arguments into a callee's locals 3% where calls are frequent.

**3. An app's cost is mostly not `update` and `view`.** `app.helix` executes 324 thousand
bytecode ops in 797 M instructions. Where they go:

| | share of `app.helix` |
|---|--:|
| parsing, checking, folding and compiling the program and `std.ui` (3,600 lines), before the first event | 66% |
| — of which `jit::analysis::recursive_funcs`, called 7 times, each asking of every pair of functions "does this body call that name" | 21% |
| — of which the parser copying its table of function signatures once per string interpolation hole (`parse_expression_at`, 472 copies) | 8% |
| — of which "did you mean" suggestions for errors the constant folder then discards (`suggest::hint` under `fold`) | 7% |
| `view::view_html`: turning the view's records into HTML, in Rust | 24% |
| — of which `view::get`, reading a field of a record by NAME: a lock on the symbol table and a string comparison per field | 10% |
| bytecode ops of `update`, `view` and `std.ui`'s components | 9% |

`gallery` (3.17 M ops) is the same picture with more rendering: 37% before the VM starts,
`view::get` 9%, `recursive_funcs` 15%.

## What landed

Each change is one commit. Ir before and after is against the commit before it, so the
columns chain.

### 1. The dispatch loop runs inside the error-catching closure (`src/vm.rs`, `exec`)

The closure that catches an op's error now contains the loop instead of being called from
it: it is entered once per program, and once more after each error a `try` catches. An arm
that has finished its op says `continue 'ops` where it said `return Ok(())`. Nothing else
moved — the same arms, the same order of effects, the same handler code after the closure
returns.

| workload | before | after | |
|---|--:|--:|--:|
| records | 915,916,770 | 675,010,312 | −26.3% |
| nested | 966,257,794 | 724,170,153 | −25.1% |
| strings | 438,411,182 | 360,570,135 | −17.8% |
| closures | 839,913,592 | 582,900,737 | −30.6% |
| match | 1,207,515,087 | 849,779,962 | −29.6% |
| dicts | 585,376,475 | 479,890,145 | −18.0% |
| methods | 1,036,999,312 | 835,859,956 | −19.4% |
| calls | 1,098,008,673 | 775,959,868 | −29.3% |
| app | 796,794,811 | 777,411,410 | −2.4% |
| gallery | 2,153,461,600 | 1,963,081,740 | −8.8% |
| orbit | 904,344,434 | 857,433,041 | −5.2% |
| freezer_model | 1,045,952,647 | 769,213,464 | −26.5% |
| updates_model | 1,408,729,403 | 1,027,906,964 | −27.0% |
| lockout_model | 613,204,382 | 465,716,782 | −24.1% |

Suite (56 programs): 8,145,977,315 → 6,196,177,775, **−23.9%**. The largest:
`statistics/simulation` −29.2%, `language/recursion` −27.9%, the three rule model tests −24%
to −27%, `statistics/nist` −13.8%. No program got slower beyond run-to-run noise, which for
the smallest programs (3 M instructions, all start-up) is ±0.5%: the largest plus sign,
`bio/alignments` at +0.7%, measures 3,357,290–3,380,215 on this build and
3,362,374–3,369,590 on the one before across three runs each.

Per op, the loop's own cost went from 65 instructions to about 35 (measured on a
record-building loop: fetch the frame, bounds-check the function and the instruction
pointer, load the instruction and its source position, store the advanced pointer, jump).
