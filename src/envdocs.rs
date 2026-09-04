//! The catalog of ENVIRONMENT VARIABLES — the third thing with no name to look up.
//!
//! **Why this file exists.** `helix search` learned the API catalog, then the language
//! forms, and a whole subsystem was still invisible: seventeen `HELIX_*` variables,
//! including the CAPABILITY SANDBOX. A field report established that the sandbox is
//! complete — default-deny, every effectful builtin gated, denials naming the authority
//! they needed — and that the only way to find out is to grep the compiler.
//! `helix search sandbox`, `helix search HELIX_CAP` and `helix search environment` all
//! answered nothing.
//!
//! A security feature nobody can find is a security feature nobody uses. That is a worse
//! failure than the two this project has already paid for — a `scan` that `helix doc
//! Array` printed all along, and raw strings that were syntax rather than a builtin —
//! because here the cost of not finding it is not a slow program, it is an ungated one.
//!
//! **The catalog is COMPLETE BY CONSTRUCTION, not by diligence.** A test scans `src/` for
//! every `HELIX_*` literal the source actually reads and fails if one has no entry here,
//! or if an entry names a variable nothing reads. Both directions, like
//! `every_method_has_a_doc_entry_and_no_orphans` — because a catalog that merely happens
//! to be right today is a catalog that is wrong next month.

/// One environment variable, as a reader needs to understand it.
pub struct EnvDoc {
    /// The variable's name, exactly as it is read from the environment.
    pub name: &'static str,
    /// The accepted values, as prose: `off | audit | enforce`.
    pub values: &'static str,
    /// What happens when it is UNSET — the answer a reader needs first, and the one a
    /// name alone can never give.
    pub default: &'static str,
    /// One sentence of what it does.
    pub doc: &'static str,
    /// The surprise worth knowing before use, plus the words a searcher would arrive with
    /// — the same job `notes` does in the API and syntax catalogs, and the reason
    /// `sandbox` finds `HELIX_CAP` even though the name does not contain the word.
    pub notes: &'static str,
}

/// Every variable a user may set, in rough order of how much not knowing it costs.
///
/// INTERNAL SENTINELS ARE DELIBERATELY ABSENT and the drift guard knows their names, so
/// their absence is a decision rather than an oversight — documenting a private marker as
/// configuration would invite someone to set it.
pub static ENV: &[EnvDoc] = &[
    EnvDoc {
        name: "HELIX_CAP",
        values: "off | audit | enforce",
        default: "off — no checks, byte-identical to a build without the sandbox",
        doc: "The capability sandbox's mode: whether authority-bearing operations are checked at all.",
        notes: "THE SECURITY ONE, and the reason this catalog exists — it was discoverable only by grepping the compiler. `audit` computes the deny-by-default decision and LOGS a would-be denial to stderr while still allowing it, so a program's real authority footprint can be harvested before anything is enforced; `enforce` denies. An UNRECOGNISED VALUE IS REFUSED with exit 2, not treated as `off` — a typo in a Dockerfile or a systemd unit used to disable the sandbox silently, which is a control that fails open on a misspelling. `HELIX_CAP=` (empty) is still `off`, because that is how a shell unsets an inherited variable. Keywords: sandbox, capability, permission, deny, grant, confine, isolate, security, authority, untrusted. Case-sensitive and untrimmed, so `Enforce` and `enforce ` are refused too. A denial is a CATCHABLE error, not a fatal stop — a program can `try` it and degrade — which is what makes `audit` an exact zero-risk preview of what `enforce` would break.",
    },
    EnvDoc {
        name: "HELIX_ALLOW_FS",
        values: "read | write | all",
        default: "deny — when a mode is set, nothing is granted",
        doc: "Grants filesystem authority under `HELIX_CAP=audit|enforce`.",
        notes: "`write` grants writing ONLY — it does not imply read; use `all` for both. Ungranted, `read_text`, `read_csv`, `read_dir`, `file_exists`, `mkdir`, `remove_file` and the `write_to`/`write_csv` family are refused by name: `capability denied: `write_to` needs `fs-write` authority, which is not granted`. Phase 1 is coarse on/off; path scoping arrives with cap-std (ADR 0021). Keywords: sandbox, filesystem, permission, grant, read only. It is UNSCOPED: `read` grants read_text(\"~/.ssh/id_rsa\") exactly as much as a data file, and covers read_dir and file_exists (reconnaissance) plus sqlite_query. A value that does not parse is REFUSED at startup rather than silently granting nothing.",
    },
    EnvDoc {
        name: "HELIX_ALLOW_NET",
        values: "on | all",
        default: "deny — when a mode is set, nothing is granted",
        doc: "Grants network authority under `HELIX_CAP=audit|enforce`.",
        notes: "Covers `listen`, `http_get`, `http_post`, `http_request`, `http_stream` AND the connection methods `accept`/`poll`/`respond`/`sse`/`send`, so a server is gated end to end rather than only at the door. Coarse: any host, any port. Keywords: sandbox, network, permission, grant, offline, egress. All-or-nothing across every host and port, INBOUND AND OUTBOUND ON ONE SWITCH — `on` permits http_get to anywhere and listen on any port. ADR 0021 describes a host:port allowlist; that is the eventual design, so a hostname here is REFUSED at startup rather than silently denying.",
    },
    EnvDoc {
        name: "HELIX_ALLOW_PROCESS",
        values: "on | all",
        default: "deny — when a mode is set, nothing is granted",
        doc: "Grants subprocess authority (`run`) under `HELIX_CAP=audit|enforce`.",
        notes: "Until 2026-08-28 this could not be granted AT ALL — the env path hardcoded process authority to false, so turning the sandbox on broke every program that shells out and the only remedy was turning it back off. Note what the grant cannot promise: the child is a separate program with its own permissions, so this is a boundary EXIT rather than confinement (ADR 0037 D3). Keywords: sandbox, subprocess, spawn, shell, exec, grant. Granting it is closer to granting everything than it looks: run(\"sh\", …) reaches whatever the child may reach, including the fs and net you just declined.",
    },
    EnvDoc {
        name: "HELIX_ALLOW_DB",
        values: "write | all",
        default: "deny — when a mode is set, nothing is granted",
        doc: "Grants database WRITE authority (`postgres_execute`, `postgres_open(url, \"write\")`) under `HELIX_CAP=audit|enforce`.",
        notes: "A write is its own grant (ADR 0047): `postgres_query` spends `net`, and a session that can write spends `db-write` AS WELL, so `HELIX_ALLOW_NET=on` alone keeps a program read-only against every database it can reach — the server holds a query session read-only from its first byte, and only this grant lets a session be opened without that default. `execute` on a connection is gated by the same name. There is no `read` value: reads are the `net` grant. A value that does not parse is REFUSED at startup. Keywords: sandbox, database, postgres, write, insert, update, delete, permission, grant.",
    },
    EnvDoc {
        name: "HELIX_NOJIT",
        values: "any value (presence is what counts)",
        default: "unset — the JIT runs where the build supports it",
        doc: "Turns the Cranelift JIT off, so the bytecode VM executes everything.",
        notes: "A DIFFERENTIAL SWITCH, not a tuning knob: the three engines are held byte-identical, so this changes speed and never answers. That is what makes it the oracle's lever — `helix test` and the gate run the suite three ways with this and HELIX_NOVM. `helix jit-explain` reports what the JIT was asked and answered. Keywords: jit, disable, engine, oracle, differential, debug, compile.",
    },
    EnvDoc {
        name: "HELIX_NOVM",
        values: "any value (presence is what counts)",
        default: "unset — the bytecode VM runs",
        doc: "Turns the bytecode VM off, so the tree-walking interpreter executes everything.",
        notes: "The slowest engine and the semantic reference the other two are held to. Same rule as HELIX_NOJIT: speed changes, answers do not. Keywords: interpreter, tree walker, disable, engine, oracle, differential, debug.",
    },
    EnvDoc {
        name: "HELIX_DF_ENGINE",
        values: "polars | native",
        default: "native (the shipped engine); polars only when explicitly asked for",
        doc: "Selects the DataFrame backend in a build that carries more than one.",
        notes: "A build without the requested engine REFUSES rather than silently answering with the other one — which is what lets `scripts/dfdiff.sh` trust that it is really comparing two engines. An empty value means no preference. The two are held byte-identical across every tracked program, so this is a performance and coverage choice, not a semantic one. Keywords: dataframe, backend, polars, native, engine, differential.",
    },
    EnvDoc {
        name: "HELIX_PATH",
        values: "a list of directories, OS-separated (`:` on unix, `;` on Windows)",
        default: "unset — only the entry's project and the install-relative stdlib are searched",
        doc: "Extra roots searched for a non-local import.",
        notes: "Searched BEFORE the install-relative standard-library locations, so a directory here shadows a stdlib module of the same path — which is how a local copy is tested, and how one is shadowed by accident. Keywords: import, module, search, library, include, resolve.",
    },
    EnvDoc {
        name: "HELIX_THREADS",
        values: "a positive integer; `1` runs fully serial",
        default: "unset — one worker per core",
        doc: "Caps the worker threads used for parallel array work.",
        notes: "RESULTS DO NOT DEPEND ON IT, and that is a guarantee rather than an observation: parallel map/filter are elementwise, float reductions are never reassociated (that would change the last bits and break the three-engine oracle), and the nested reduce partitions over independent outer indices and collects in order. So this is a pure CPU-versus-latency control — measured on a 50M dot product, the default spends about 2x the CPU to finish 1.44x sooner, which is the wrong trade on a shared box. An invalid value leaves the default. Keywords: parallel, threads, cores, serial, rayon, cpu, contention.",
    },
    EnvDoc {
        name: "HELIX_STACK_MB",
        values: "a positive integer, in MiB",
        default: "128 MiB in a release build, 1 GiB in a debug build",
        doc: "The stack given to the evaluation thread.",
        notes: "For the rare program that recurses deep with large frames. A value that does not parse, or is zero, leaves the default rather than refusing — the parse is a filter, not a validator. Keywords: stack, recursion, overflow, depth, memory.",
    },
    EnvDoc {
        name: "HELIX_CACHE",
        values: "a directory path",
        default: "the platform cache directory",
        doc: "Where fetched package tarballs are cached.",
        notes: "An EMPTY value is ignored rather than meaning \"the current directory\", which is the shell idiom this and HELIX_DF_ENGINE both honour. Keywords: package, cache, download, tarball, dependency, offline.",
    },
    EnvDoc {
        name: "HELIX_RICH",
        values: "1 | always | 0 | never",
        default: "auto — rich only when stdout is a terminal",
        doc: "Forces rich output on or off, overriding TTY detection.",
        notes: "The switch to reach for when piping to a file or a CI log and you still want (or still do not want) tables and colour. Colour additionally requires that NO_COLOR is unset and HELIX_COLOR is not `never`, so plain output has three independent ways to be requested. Keywords: tty, terminal, plain, pretty, format, output, ci, pipe.",
    },
    EnvDoc {
        name: "HELIX_COLOR",
        values: "never (anything else is ignored)",
        default: "unset — colour follows rich output",
        doc: "Suppresses ANSI colour while leaving rich layout intact.",
        notes: "`NO_COLOR` (the cross-tool convention) does the same thing and is honoured too. Only `never` is meaningful; there is no value that forces colour on — use HELIX_RICH for that. Keywords: color, colour, ansi, no_color, plain, accessibility.",
    },
    EnvDoc {
        name: "HELIX_THEME",
        values: "default | vivid | ocean | warm | mono",
        default: "default",
        doc: "The colour palette for rich output.",
        notes: "An unknown name falls back to `default` rather than refusing. `mono` is the one to reach for when colour is wanted but hue is not meaningful. Keywords: theme, palette, colour, style, appearance, dark, light.",
    },
    EnvDoc {
        name: "HELIX_BOX",
        values: "rounded | square | ascii | none",
        default: "rounded",
        doc: "The box-drawing style for tables.",
        notes: "`ascii` is the one that survives a terminal or a log pipeline without Unicode box characters; `none` drops the borders entirely and keeps the alignment. An unknown name falls back to `rounded`. Keywords: table, border, box, unicode, ascii, frame, layout.",
    },
    EnvDoc {
        name: "HELIX_PLOT",
        values: "braille | blocks | ascii",
        default: "braille",
        doc: "The glyphs a chart draws with.",
        notes: "Reach for this when a plot looks sheared or scattered: the rows a chart emits are always the same width, so a ragged plot means the terminal FONT renders braille at a different width from the braille blank it is padded with. `blocks` needs only the quarter-block characters, which far more fonts have; `ascii` needs nothing at all and still keeps the vertical position, so a rising curve still rises. Resolution falls 2x4, then 2x2, then 1x4 per cell. An unknown name falls back to `braille`. Keywords: chart, plot, braille, sparkline, glyph, font, sheared, misaligned, unicode, ascii.",
    },
];

/// Variables the source reads that are NOT user-facing configuration, with the reason.
///
/// The drift guard consults this list, so adding a `HELIX_*` read to the source forces a
/// choice: give it a catalog entry, or say here why it does not deserve one. Silence is
/// the one option it removes.
pub static INTERNAL: &[(&str, &str)] = &[(
    "HELIX_UDF_ERR",
    "not an environment variable at all — a Rust `const` naming the sentinel prefix that \
     carries a Helix error out of a polars UDF and back into the language's own message. \
     It is read from a `PolarsError` string, never from the environment, and documenting \
     it as configuration would invite someone to set it.",
)];

/// The entry for `name`, if it is one.
pub fn env_doc(name: &str) -> Option<&'static EnvDoc> {
    ENV.iter().find(|e| e.name == name)
}
