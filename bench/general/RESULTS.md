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

### 2. The JIT's call graph is built in one walk per function (`src/jit/analysis.rs`)

Not a VM change: this is start-up, and it is here because the profile put it first for
every app. `recursive_funcs` and `on_mutual_cycle` asked `body_calls(body, name)` of every
pair of functions — each body walked once per function in the program — and the graph is
wanted seven times per compile, plus once per self-recursive function. A program that
imports `std.ui` has some 300 functions. `call_graph` now walks each body once, collecting
the names it calls (`callees`, the same node kinds `body_calls` visits), and reads the edges
off a name index. The graph is the same graph: a test builds it both ways for `std/ui.helix`
and four examples and compares, and `helix jit-explain` prints the same decisions for 61
programs (examples, kernels, apps, `std/ui.helix`) before and after.

| workload | before | after | |
|---|--:|--:|--:|
| app | 777,411,410 | 584,425,752 | −24.8% |
| gallery | 1,963,081,740 | 1,632,419,562 | −16.8% |
| orbit | 857,433,041 | 592,608,289 | −30.9% |
| lockout_model | 465,716,782 | 463,417,315 | −0.5% |
| freezer_model | 769,213,464 | 766,842,820 | −0.3% |
| the other nine | | | −0.1% to 0.0% |

Suite: 6,196,177,775 → 6,188,976,380, −0.12%; `rules/freezer_test` −13.7%. The examples are
small programs, where the graph was never expensive, so what matters there is that it did
not get dearer: a first version that kept a hash set of names per body made the programs
with a handful of functions 2% slower (`rules/lockout` 3,487 k → 3,567 k, repeatably) and
was replaced by one table and one scratch list per graph (`rules/lockout` 3,446 k). The
largest plus signs left — `dataframes/io` +1.2%, `bio/annotations` +0.6% — are programs of
2–3 M instructions that measure 3,404–3,442 k and 2,463–2,516 k on EITHER build across
three runs each: start-up noise, about ±1% at that size.

### 3. A view node's props are named once, not once per prop (`src/view.rs`, `parse`)

`view_html` looks for each of the forty props it knows on every node, and each look
(`view::get`) compared the name with every field's name as text — which means asking the
symbol table for the text, a lock taken and released per field per prop. `Props` asks once
per field per node and keeps the names. Dict props and anything else go through `get` as
before; a test asks both the same questions of a record (with a `missing` field and a
repeated name), a dict, and two non-containers. The five example apps replay to the same
bytes on all three engines.

| workload | before | after | |
|---|--:|--:|--:|
| app | 584,425,752 | 545,347,349 | −6.7% |
| gallery | 1,632,419,562 | 1,542,424,901 | −5.5% |
| orbit | 592,608,289 | 582,982,608 | −1.6% |
| the other eleven | | | 0.0% |

Suite: 6,188,976,380 → 6,188,703,266, 0.00% (no example renders a view).

**Tried first and not kept:** resolving the prop's name to its symbol (`Symbol::lookup`) and
comparing symbols. One lookup — lock, hash the name, compare the text — costs more than the
two or three per-field lock-and-compares it replaces on a typical props record: app +3.2%,
gallery +5.0%, orbit +1.3%.

### 4. The running frame's instruction pointer, code and locals base are locals (`src/vm.rs`, `exec`)

After change 1 the loop's own cost was about 35 instructions per op, most of it re-deriving
what does not change between a call and a return: index the top frame, bounds-check its
function, load the instruction pointer, bounds-check it against the code and again against
the position table, store the advanced pointer back. The loop is now two loops: `'frame`
loads `ip`, the code slice and the locals base into locals; `'ops` runs instructions against
them; an arm that pushes, pops or re-points a frame says `continue 'frame`. A jump is an
assignment to a local. The one obligation it creates — an arm that PUSHES a frame must
write `ip` back first, because that is where its `Return` resumes — is at five places
(the two pushes in `enter_call!`, `CallValue`, the two function-valued-field calls in
`Method`), and `tests/corpus/vm_frames_resume.helix` runs every way into and out of a
frame, each followed by more work in the caller, on all three engines against a golden
taken from the tree-walker of the branch point.

| workload | before | after | |
|---|--:|--:|--:|
| records | 674,301,496 | 621,669,816 | −7.8% |
| nested | 723,463,396 | 673,158,053 | −7.0% |
| strings | 360,562,711 | 340,386,190 | −5.6% |
| closures | 582,767,879 | 535,084,964 | −8.2% |
| match | 849,511,009 | 758,735,263 | −10.7% |
| dicts | 479,732,598 | 451,172,704 | −6.0% |
| methods | 835,873,282 | 788,575,844 | −5.7% |
| calls | 775,615,361 | 714,413,679 | −7.9% |
| app | 545,347,349 | 542,505,454 | −0.5% |
| gallery | 1,542,424,901 | 1,506,669,535 | −2.3% |
| orbit | 582,982,608 | 574,595,513 | −1.4% |
| freezer_model | 766,825,196 | 695,592,299 | −9.3% |
| updates_model | 1,026,482,717 | 921,398,363 | −10.2% |
| lockout_model | 463,408,973 | 428,832,332 | −7.5% |

Suite: 6,188,703,266 → 5,692,717,479, **−8.0%** (`statistics/simulation` −10.3%, the rule
model tests −7.5% to −10.2%, `language/recursion` −9.7%); the largest plus sign is
`numerics/tensors` +0.85%, a 3.2 M-instruction program inside the ±1% start-up noise.

### 5. `p.f` on a local is one instruction that reads the field in place (`Op::LoadLocalField`)

`LoadLocal; GetField` was the commonest pair of instructions wherever records are read. The
pair pushed a clone of the record, searched it, cloned the field and dropped the record;
the fused instruction borrows the slot and calls the same `eval_field`, so the value, the
`missing` pass-through and both errors (with their hints and the field's position) are the
walker's own. The compiler fuses only what it emitted as exactly `LoadLocal` for a bare
identifier — an upvalue or a global keeps the two-instruction form.
`tests/corpus/local_field_read.helix` pins all of it on three engines, golden from the
branch point's tree-walker.

| workload | before | after | |
|---|--:|--:|--:|
| records | 621,669,816 | 586,171,763 | −5.7% |
| nested | 673,158,053 | 627,945,719 | −6.7% |
| strings | 340,386,190 | 340,376,898 | 0.0% |
| closures | 535,084,964 | 516,308,278 | −3.5% |
| match | 758,735,263 | 756,556,909 | −0.3% |
| dicts | 451,172,704 | 451,328,841 | 0.0% |
| methods | 788,575,844 | 788,641,809 | 0.0% |
| calls | 714,413,679 | 673,945,671 | −5.7% |
| app | 542,505,454 | 539,609,805 | −0.5% |
| gallery | 1,506,669,535 | 1,499,133,365 | −0.5% |
| orbit | 574,595,513 | 572,548,893 | −0.4% |
| freezer_model | 695,592,299 | 678,472,990 | −2.5% |
| updates_model | 921,398,363 | 876,058,050 | −4.9% |
| lockout_model | 428,832,332 | 407,870,112 | −4.9% |

Suite: 5,692,717,479 → 5,608,709,090, **−1.5%** — the rule model tests (−2.5% to −4.9%) and
nothing else of size; the largest plus sign is `bio/annotations` +1.5%, the 2.5 M program
whose run-to-run range is 2,463–2,516 k on any build.

This is what "field access" was worth, and it needed no shapes and no cache: the cost was
never the search (a record here has two to five fields) but the two dispatches and the
record's reference count going up and down around it.

### 6. A record is built straight from the stack (`Op::MakeRecord`, `Op::UpdateRecord`)

Both first copied their operands off the stack into a temporary `Vec` (`split_off`) and
then moved them again into the record's fields: an allocation and a free per record, on
top of the two the record needs. `MakeRecord` now zips the names with a `drain` of the
stack; `UpdateRecord` reads the base where it sits and drains the values above it. A
refused spread now leaves its operands on the stack for the unwind to truncate rather than
dropping them first — `Op::Interp` already works that way.
`tests/corpus/record_build_update.helix` pins what comes out (which part wins, the base
unchanged, a caught and an uncaught refused spread) on three engines.

| workload | before | after | |
|---|--:|--:|--:|
| records | 586,171,763 | 561,294,646 | −4.2% |
| nested | 627,945,719 | 593,041,358 | −5.6% |
| closures | 516,308,278 | 504,958,161 | −2.2% |
| updates_model | 876,058,050 | 864,887,694 | −1.3% |
| calls | 673,945,671 | 665,917,447 | −1.2% |
| gallery | 1,499,133,365 | 1,482,962,087 | −1.1% |
| lockout_model | 407,870,112 | 403,991,099 | −1.0% |
| app | 539,609,805 | 534,930,911 | −0.9% |
| dicts | 451,328,841 | 447,557,036 | −0.8% |
| match | 756,556,909 | 750,727,711 | −0.8% |
| freezer_model | 678,472,990 | 675,486,039 | −0.4% |
| orbit | 572,548,893 | 570,837,780 | −0.3% |
| strings, methods | | | 0.0% |
