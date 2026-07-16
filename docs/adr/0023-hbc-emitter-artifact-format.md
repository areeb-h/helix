# ADR 0023 — The `.hbc` emitter and artifact format (Helix Bytecode Container v0)

- **Status:** Accepted — **built and verified end-to-end.** The emitter (`helix emit-hbc`),
  the host oracle, and ring-0 execution all ship; a Helix-compiled `.hbc` runs in ctype's
  kernel and its result matches both the host VM and the hand-assembled demo (serial output
  quoted below). Only the **minimal core subset** is in scope for v0; everything else is a
  precise rejection today and a future `.hbc` version later.
  **Amendment 2026-07-10:** `/` removed from the emitter's scope. Helix `/` is *float*
  division with Int→Float promotion; hvm's `DIV` integer-divides Ints (and yields `inf`
  on a `0.0` divisor where Helix raises) — emitting it made the same blob compute `7 / 2`
  as `3` in ring 0 vs `3.5` on the host, a proven divergence the seam forbids. The
  emitter now rejects `/` (regression test `rejects_division`); hvm's `DIV` selector
  remains in the *format* for the hand assembler. Division re-enters emitter scope when
  hvm gains Int→Float promotion (see Open questions).
- **Date:** 2026-07-10
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0021 — Capability sandbox](0021-capability-sandbox.md) (the emitter is a
  pure, capability-free lowering — it reads a compiled `Program` and writes bytes, touching no
  `Net`/`Fs` surface). The consumer side lives in the **ctype** repo: ctype ADR 0010 (Helix as
  ctype's ring-0 execution substrate) and `ctype/docs/helix-bridge.md` (the cross-repo status
  mirror). **This ADR is the single authoritative source for the `.hbc` byte layout;** ctype
  ADR 0010 and `helix-bridge.md` link here and must not restate the format.

## Context

Helix is a from-scratch scientific language with a small set of load-bearing principles: **one
obvious way** to do a thing, **immutable by default**, **zero-copy** where the data model allows
it, and **great, source-attributed errors** over silent coercion. Until now Helix compiled to an
in-process bytecode `Program` and executed it in its own VM — but it had **no artifact format**:
no portable, serializable thing you could write to disk, hand to another process, or run somewhere
Helix itself is not installed.

Two forces made an artifact format necessary now:

1. **ctype needs an execution substrate.** ctype embeds `hvm`, a `no_std`, zero-allocation core
   VM, and runs bytecode in **ring 0** (ctype ADR 0010). For Helix to *be* that substrate, a Helix
   program has to cross the repo boundary as **bytes that `hvm::Program::parse` accepts** — not as
   an in-memory Rust structure. That is a serialized artifact by definition.
2. **Helix wants an artifact anyway.** A portable, self-describing bytecode container is the thing
   you cache, ship, diff, and re-run. It is the natural unit for "compile once, run anywhere the
   VM runs," and it is a prerequisite for every later story (AOT caching, distribution, a JIT that
   consumes the same IR). Adding it in service of ctype also fills a real Helix-side gap.

The container is called **HBC** — *Helix Bytecode Container* — and this ADR specifies **v0**. The
principles constrain the shape directly: **one obvious way** → exactly one emitter subcommand and
exactly one byte layout; **immutable / no surprises** → a fixed-width, position-computable format
with a magic and a version, no optional trailing junk; **zero-copy** → little-endian fixed-width
fields the `no_std` consumer can read in place without an allocator; **great errors** → anything
outside the encodable subset is a *source-attributed* compile error, never a silently dropped
instruction.

## Prior approaches and their documented shortcomings

The design deliberately learns from where established bytecode artifacts hurt. HBC v0 is the small
deliberate opposite of each documented pain:

| Format | Approach | Documented pain we avoid |
|---|---|---|
| **WebAssembly** | Portable stack VM, LEB128-encoded, structured sections, growing proposal set (GC, SIMD, tail-calls, exceptions, component model) | Genuinely portable, but the spec surface and toolchain are **huge**; variable-length LEB128 and a validating structured decoder are a lot of machinery for a ring-0 core. HBC keeps **fixed-width little-endian** fields so a `no_std` VM parses in place with no allocator and no validator zoo. |
| **Python `.pyc`** | Marshal of a `CodeObject`, prefixed with a **magic tied to the exact interpreter version** | `.pyc` is **not portable across interpreter versions** and its marshal format is an undocumented implementation detail. HBC has an **explicit `u32` version** in the header and a documented layout (this ADR) so a mismatch is a clean, detectable refusal, not undefined behavior. |
| **Erlang `.beam`** | IFF/chunk container: named chunks (`Atom`, `Code`, `ImpT`, `ExpT`, `LitT`, …), some compressed | Powerful but **heavy**: a chunked, sometimes-compressed container with an atom table is far more than a core VM needs. HBC is **one linear stream** — header, one global constant pool, one function table, one code blob — no chunk directory, no compression, no atoms. |
| **JVM `.class`** / protobuf-style tag-length-value | Per-class constant pool + typed, tagged, sometimes length-delimited entries; verifier-dependent | Per-unit constant pools and TLV framing bring **schema/verifier complexity and cross-unit fragmentation**. HBC has **one program-global constant pool** of only `Int/Float/Bool` and **fixed-size** records — no per-function pool to reconcile at load time, no length-delimited walking. |

The through-line: existing formats pay for generality with **variable-length encodings, per-unit
pools, chunk directories, compression, and version-coupled opacity**. A ring-0, zero-allocation
consumer wants none of that. HBC v0 buys portability with the *minimum* structure that is still
honest (a magic + a version) and nothing more.

## Decision

Add a **hand-rolled emitter** that lowers a compiled Helix `Program` to the `.hbc` v0 byte format,
exposed as the CLI subcommand **`helix emit-hbc`**, producing an artifact **byte-exact to what
ctype's `hvm::Program::parse` accepts**.

### CLI surface

```
helix emit-hbc <script> [--entry NAME] [-o out.hbc] [--dump]
```

Pipeline: `module::load` → `types::check` → `bytecode::compile_with_types` → `hbc::emit` → write
bytes. `--entry` selects the entry function (default the module's), `-o` the output path, `--dump`
prints a human-readable disassembly. Because `.hbc` has **no name table**, `emit-hbc` prints the
**function-index table** to stdout (the entry is always index 0). This is the *one* spelling — not
`helix build --target=hbc`.

### The `.hbc` v0 byte layout (authoritative)

All integers are **little-endian**. The stream is one linear sequence with no padding:

```
┌─ Header ──────────────────────────────────────────────────────────────┐
│ offset 0   magic     4 bytes   ASCII "HBC0"                            │
│ offset 4   version   u32       = 0                                     │
│ offset 8   nconsts   u32       number of constant-pool entries         │
├─ Constant pool ── nconsts entries × 9 bytes each ─────────────────────┤
│   tag      u8    1 = Int, 2 = Float, 3 = Bool                         │
│   payload  u64   Int: i64 stored as u64 bits                          │
│                  Float: f64::to_bits                                   │
│                  Bool: 0 or 1                                          │
├─ Function count ──────────────────────────────────────────────────────┤
│   nfuncs   u32                                                         │
├─ Function table ── nfuncs entries × 16 bytes each ────────────────────┤
│   nargs    u32                                                        │
│   nlocals  u32                                                        │
│   code_off u32   byte offset into the code blob (cumulative from 0)   │
│   code_len u32   length in bytes of this function's code              │
├─ Code blob ── each function's code concatenated, in table order ──────┤
│   opcodes, see below                                                  │
└───────────────────────────────────────────────────────────────────────┘
```

`code_off` is **relative to the start of the code blob** and cumulative (function 0 starts at 0,
function *k* starts where function *k−1* ended). The pool is **program-global** and holds only
scalar `Int/Float/Bool` — there are no strings and no aggregate constants in v0.

**Opcode encoding** — each instruction is a 1-byte opcode followed by little-endian operands:

| Opcode | Byte | Operands |
|---|---|---|
| `CONST` | `0x01` | `u32` constant-pool index |
| `LOAD_LOCAL` | `0x02` | `u32` local slot |
| `STORE_LOCAL` | `0x03` | `u32` local slot |
| `BINARY` | `0x04` | `u8` selector |
| `JUMP` | `0x05` | `u32` **byte** target (chunk-relative) |
| `JUMP_IF_FALSE` | `0x06` | `u32` **byte** target (chunk-relative) |
| `CALL_FN` | `0x07` | `u32` function index, `u32` nargs |
| `TAIL_CALL_FN` | `0x08` | `u32` function index, `u32` nargs |
| `RETURN` | `0x09` | — |
| `POP` | `0x0A` | — |
| `CALL_HOST` | `0x0B` | `u32` host index, `u32` nargs |

**Binary selectors:** `ADD 0`, `SUB 1`, `MUL 2`, `DIV 3`, `LT 4`, `LE 5`, `GT 6`, `GE 7`,
`EQ 8`, `NE 9`. VM value types: `Int(i64)`, `Float(f64)`, `Bool`. No strings.

**Host ABI (added 2026-07-10).** `CALL_HOST` pops `nargs` values and invokes the runtime's
host handler with `(host_idx, args)`, pushing its result — the seam through which a `.hbc`
program reaches the outside world (a kernel capability). The stable host indices the
emitter produces, and the Helix builtins that lower to each:

| host idx | Helix builtins | effect | ctype capability |
|---|---|---|---|
| `0` | `print`, `emit`, `elog` | write the value + newline; return it | `CAP_PRINT` |
| `1` | `sleep` | pause for the argument's worth of time; return it | `CAP_SLEEP` |
| `2` | `read_int` | read one integer from the console; return it | `CAP_GETKEY` |

(On a single serial console the `print`-rich vs `emit`-streaming distinction collapses to
"write a line", so all three output builtins share host `0`.) With no handler, `CALL_HOST`
is a clean `Error::NoHost`; a handler that refuses (unknown index, or the caller lacks the
capability) returns `Error::HostDenied` — never a fault, so totality holds as long as the
handler does. ctype's kernel gates each host function on the invoking task's capabilities
(ctype ADR 0011). The ABI grows as more builtins map to capabilities.

**Worked example — `compute.hbc` (63 bytes), from `fn compute() = 2 + 3`:**

```
[0..4)   48 42 43 30            "HBC0"
[4..8)   00 00 00 00            version = 0
[8..12)  02 00 00 00            nconsts = 2
[12..21) 01  02 00 00 00 00 00 00 00    const[0] = Int 2
[21..30) 01  03 00 00 00 00 00 00 00    const[1] = Int 3
[30..34) 01 00 00 00            nfuncs = 1
[34..50) 00 00 00 00  00 00 00 00  00 00 00 00  0D 00 00 00
                                    func[0]: nargs=0 nlocals=0 code_off=0 code_len=13
[50..63) 01 00000000  01 01000000  04 00  09
                                    CONST 0 ; CONST 1 ; BINARY ADD ; RETURN
```

12 (header) + 18 (2×9 pool) + 4 (nfuncs) + 16 (1×16 table) + 13 (code) = **63 bytes**, matching
the emitted artifact exactly.

### Scope: the minimal, dependency-free core subset

v0 lowers **only** the subset ctype's `hvm` ISA can execute today:

- `Int`/`Float`/`Bool` arithmetic and comparisons (`+ - * < <= > >= == !=` — **not** `/`,
  see the Status amendment: Helix `/` float-promotes and hvm's `DIV` does not, so
  emitting it would silently diverge);
- frame locals (`LoadLocal`/`StoreLocal`);
- the Helix **superinstructions** (`LoadLocalBinary`, `LoadLocalConstBinary`, `ConstBinary`),
  expanded back to primitive `LOAD_LOCAL`/`CONST`/`BINARY` sequences;
- `if`/`while` control flow (`Jump`/`JumpIfFalse`);
- direct and tail calls (`CallFn`/`TailCallFn`), `Return`, `Pop`;
- the **host-mapped builtins** (2026-07-10): `print`/`emit`/`elog` → `CALL_HOST 0`
  (output), `sleep` → `CALL_HOST 1` (pause), and `read_int` → `CALL_HOST 2` (console
  input), per the Host ABI table above — so a `.hbc` program can print, pace itself,
  and read input through host-mediated capabilities. Other builtins remain rejected.

Everything else is **rejected with a precise, source-attributed error**, never silently dropped:
division (`/` — see the Status amendment; `//` and `%` likewise), globals, unary (`-x`/`!x`),
short-circuit `and`/`or`/`??`, closures and first-class functions, arrays/records/tuples and
indexing, string interpolation, non-`print` methods/builtins, `match`, `try`/`raise`,
DataFrame/GroupBy verbs, and any non-scalar constant (only `Int`/`Float`/`Bool` are encodable).

### The two representation reconciliations

The emitter exists to bridge two honest mismatches between Helix's bytecode and `hvm`'s:

1. **Jump target: instruction index → byte offset.** Helix `Jump`/`JumpIfFalse` name an
   **instruction index**; `hvm` names a **chunk-relative byte offset**. The emitter does a
   **two-pass lowering**: pass one computes every instruction's byte offset within its function;
   pass two patches each jump operand to the target instruction's byte offset.
2. **Constant pool: per-chunk arbitrary → global scalar.** Helix keeps a **per-chunk** pool of
   arbitrary `Value`s; `hvm` has **one program-global** pool of only `Int/Float/Bool`. The
   emitter **interns (dedups)** each referenced scalar into the shared global pool and **remaps**
   every `CONST` index accordingly.

The emitter emits the entry function plus **everything reachable from it** via direct/tail calls,
**reindexed so the entry is at index 0** (`hvm` addresses functions by index).

## Rationale

- **Fixed-width little-endian, hand-rolled.** The consumer is a `no_std`, zero-allocation ring-0
  VM. Fixed-width LE fields are readable in place with no allocator, no LEB128 decoder, no
  validating section walker — this is the *zero-copy* principle applied to the wire format. A
  hand-rolled writer mirrors `hvm`'s hand-rolled parser exactly, byte for byte, with no version
  skew mediated by a third crate.
- **One global scalar pool, one linear stream.** No per-function pools to reconcile at load time,
  no chunk directory, no compression. This is the *one obvious way* principle: the layout is fully
  position-computable from the counts in the header.
- **Magic + explicit version.** `HBC0` + `version u32 = 0` makes a mismatch a **clean, detectable
  refusal** rather than the version-coupled opacity of `.pyc`. Growth happens by bumping the
  version, not by mutating v0.
- **Reject, don't degrade.** Emitting a partial or approximated program would violate *no
  surprises*. A source-attributed error for anything outside the core subset keeps the artifact's
  meaning exact and total.

## Rejected alternatives

- **`serde` / a serialization crate instead of hand-rolling.** Rejected. The consumer is `hvm`'s
  `no_std` hand-rolled parser; introducing `serde` (or any framing crate) on the producer side
  would couple the byte layout to a crate's conventions, add a dependency, and surrender exact
  control of the layout. Hand-rolling gives **zero dependencies, byte-exact control, and a
  producer that mirrors the consumer**. Chosen: **hand-rolled.**
- **Full-core `.hbc` (arrays, records, closures, strings) in v0.** Rejected. Those require ISA
  extensions `hvm` does not have yet (aggregate values, heap, closure representation). Shipping
  them would mean *inventing* VM semantics on the producer side with no consumer to honor them.
  Chosen: **minimal core** — arithmetic + control flow + calls — with aggregates deferred to a
  later `.hbc` version once the `hvm` ISA grows to meet them.
- **`helix build --target=hbc` spelling.** Rejected in favor of the single dedicated verb
  **`helix emit-hbc`** — one obvious way, no target-matrix ambiguity.

## Consequences

- **Commits later phases to a versioned format.** Aggregates (arrays/records/closures/strings)
  arrive as **`.hbc` v1+** paired with `hvm` ISA extensions — never by silently mutating v0. The
  magic+version header is the forward-compatibility seam.
- **The core/tail split is now a contract.** `CALL_FN` and `TAIL_CALL_FN` are distinct opcodes;
  the emitter must preserve Helix's tail-call classification so the VM can execute tail calls in
  constant stack space. Losing that distinction would regress recursion depth.
- **Cross-repo authority is fixed here.** The byte layout is specified in this ADR **once**. ctype
  ADR 0010 and `ctype/docs/helix-bridge.md` link here rather than restating it, so there is a
  single source of truth and no drift.
- **The emitter stays a pure, capability-free lowering** (ADR 0021): it consumes a compiled
  `Program` and produces bytes; it opens no sockets and reads no files beyond the input script the
  CLI already loads.
- **A host oracle exists for verification.** ctype's `hvm/src/bin/hbc.rs` gained
  `cargo run --bin hbc -- run <file.hbc> [entry] [arg]`, which parses an arbitrary `.hbc` and runs
  one function on the host VM — so `emit-hbc → .hbc → hbc run → result` is checkable **before** the
  kernel is involved.

## Verification

Done and verified end-to-end (QEMU, 2026-07-10, confirmed by serial-grep). Two artifacts —
`kernel-rs/assets/compute.hbc` (63 bytes, from `fn compute() = 2 + 3`) and `fib.hbc` (122 bytes,
from `fn fib(n) = if n < 2 then n else fib(n-1) + fib(n-2)`) — are embedded and run by
`helixvm::run_helix_compiled()` alongside the hand-assembled demo (`helixvm::run_demo()`). Exact
serial output at boot:

```
helixvm: ran .hbc in ring 0 — fib(25)=75025 sum(1..100000)=5000050000 [OK, matches host]
helixvm: ran Helix-compiled `fn compute() = 2 + 3` in ring 0 — compute()=5 [OK]
helixvm: ran Helix-compiled `fib` in ring 0 — fib(25)=75025 [OK, matches the hand-assembled demo]
```

**Cross-producer equivalence:** the Helix-compiled `fib(25)=75025` equals the hand-assembled
demo's `fib(25)=75025`. Two fully independent producers — the `hvm` hand assembler and the Helix
compiler + `emit-hbc` — agree on the result through the same byte format, which is the strongest
signal the layout in this ADR is correct.

## Open questions

- **Division needs Int→Float promotion in hvm.** Helix `/` always yields a Float (`7 / 2` is
  `3.5`) and raises on a `0.0` divisor; hvm's `DIV` does neither. To bring `/` (and `//`/`%`)
  into emitter scope, hvm needs a promotion rule (or a dedicated float-div opcode) plus the
  zero-divisor error — an ISA/semantics extension, not a container change.
- **Aggregates need an ISA before a format.** Arrays/records/closures/strings are deferred to a
  future `.hbc` version; the open question is the `hvm` value/heap model that must land first, not
  the container framing (which the version field already accommodates).
- **Function names in the artifact.** v0 has no name table; `emit-hbc` prints the index table to
  stdout. Whether a future version carries an optional symbol/debug section (for tooling and
  stack traces) is deferred.
- **Endianness/portability envelope.** v0 fixes little-endian to match the host and ctype target.
  A big-endian or width-varying target would be a new version decision, not a v0 concern.
