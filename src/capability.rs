//! The capability gate (ADR 0021, phase 1: effect categories + audit mode).
//!
//! Every authority-bearing builtin passes through [`gate`] before it runs. Phase 1 is
//! deliberately **non-breaking**: the default mode is `Off` (no checks — byte-identical to
//! pre-capability Helix), and `Audit` only *logs* would-be denials to stderr while still
//! allowing the access, so the real capability footprint of a program / its dependencies /
//! its generated code can be harvested before `Enforce` is ever the default.
//!
//! Scope of phase 1: the gated set is coarse on/off per category, driven by the environment
//! (`HELIX_CAP`, `HELIX_ALLOW_FS`, `HELIX_ALLOW_NET`). Phase 1b wires the `helix.toml`
//! `[capabilities]` block and `--allow-*` flags as the durable source of truth; later phases
//! add `cap-std`-backed path/host scoping, per-evaluation attenuation, and `helix build`
//! grant-baking (see the ADR). Methods (`write_to`/`append_to`, the `Conn` verbs) are gated
//! in a follow-up; this module gates the effectful *builtins*.

use crate::error::HelixError;
use crate::value::Value;
use std::sync::OnceLock;

/// The authority an effectful builtin exercises. The gated set is
/// `FsRead`/`FsWrite`/`Net`/`Process`/`Env`; everything else (math, `print`/`emit` output,
/// `sleep`/`clock_monotonic`, randomness, crypto key generation) is `Pure` and never gated —
/// those are effects but not *authority*.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Effect {
    Pure,
    FsRead,
    FsWrite,
    Net,
    /// Granted to `run` (ADR 0037 D3). Note what this grant CANNOT promise: the child
    /// runs as a separate program with its own permissions, so it is a boundary exit
    /// rather than confinement.
    Process,
    /// A database session that can WRITE — `postgres_execute`, `postgres_open(url, "write")`
    /// and `execute` on such a connection (ADR 0047). Finer than `Net`, which every
    /// PostgreSQL verb spends: this is the authority to change the far end, and it needs
    /// `net` as well, because the database is reached over the network.
    DbWrite,
    #[allow(dead_code)]
    Env,
}

impl Effect {
    /// Whether an access of this effect is subject to the capability gate.
    pub fn gated(self) -> bool {
        !matches!(self, Effect::Pure)
    }

    pub fn label(self) -> &'static str {
        match self {
            Effect::Pure => "pure",
            Effect::FsRead => "fs-read",
            Effect::FsWrite => "fs-write",
            Effect::Net => "net",
            Effect::Process => "process",
            Effect::DbWrite => "db-write",
            Effect::Env => "env",
        }
    }
}

/// The effect category of a builtin by name. Only the authority-bearing builtins are listed;
/// everything else is `Pure` (ungated). This is the finer axis the boolean `pure` flag in
/// the registry cannot express (`print`/`emit`/`sleep`/`aes_keygen` are `pure:false` yet
/// hold no authority).
pub fn effect_of(name: &str) -> Effect {
    match name {
        // `sqlite_query` opens the database READ-ONLY (src/db.rs), so `fs-read` is the
        // truth rather than a convenient label. A writing verb needs `fs-write` and its
        // own entry: the classification and the open mode have to agree, or the audit
        // log says one thing while the process does another.
        "sqlite_query"
        | "read_csv" | "read_parquet" | "read_text" | "read_json" | "read_dir" | "file_exists"
        | "read_fasta" | "read_fastq" | "read_vcf" | "read_bcf" | "read_sam" | "read_bam"
        | "read_gff" | "read_bed" => Effect::FsRead,
        // THE DURABLE-STORAGE SUBSTRATE (ADR 0041). `fsync` and `sync_dir` are classified
        // as WRITES even though they add no bytes: they exist only to complete a write that
        // already happened, and a program granted read-only authority has nothing to make
        // durable. `file_size` and `read_at` observe without changing and are reads.
        "remove_file" | "mkdir" | "rename" | "fsync" | "sync_dir" | "create_new"
        | "write_at" | "truncate" | "remove_dir"
        // A lock is taken to WRITE; a reader has nothing to exclude anyone from.
        | "lock_file" | "try_lock_file" => Effect::FsWrite,
        "file_size" | "read_at" | "read_bytes" | "read_bytes_at" => Effect::FsRead,
        "listen" | "http_get" | "http_post" | "http_request" | "http_stream"
        // A database query is a network verb. It only reads, but Helix has ONE
        // network authority, so `net` is what it costs and what it declares.
        | "postgres_query" | "postgres_open" => Effect::Net,
        // The first `Process` grant. ADR 0021 reserved the category and ADR 0037 D3
        // states its ceiling: a subprocess is a BOUNDARY EXIT, not confinement — it
        // runs with its own permissions, so granting `run` on a shell or on `helix`
        // itself is granting everything. The label is honest about that rather than
        // implying a guarantee the process model cannot keep.
        "run" => Effect::Process,
        "postgres_execute" => Effect::DbWrite,
        _ => Effect::Pure,
    }
}

/// The effect category of a **method** by name. The write methods (`write_to`/`append_to` on
/// String; `write_csv`/`write_tsv`/`write_json`/`write_parquet`/`write_fasta`/`write_fastq` on
/// Array-of-records / DataFrame) are `FsWrite`; the socket-touching `Conn` verbs are `Net`.
/// These names are receiver-exclusive, so keying off the name alone is unambiguous. Pure
/// methods (`map`, `mean`, `request`, `to_html`, …) are ungated.
pub fn method_effect_of(name: &str) -> Effect {
    match name {
        "write_to" | "append_to" | "write_csv" | "write_tsv" | "write_json" | "write_parquet"
        | "write_fasta" | "write_fastq" => Effect::FsWrite,
        // `stream` opens a response on the socket exactly as `sse` does, so it carries
        // the same authority. Missing a name here is not a slow path, it is an UNGATED
        // one — the gate keys on the name alone.
        "accept" | "poll" | "respond" | "sse" | "stream" | "send" => Effect::Net,
        // A write through a connection value spends what `postgres_execute` spends.
        "execute" => Effect::DbWrite,
        _ => Effect::Pure,
    }
}

/// How the gate behaves. `Off` (the default when `HELIX_CAP` is unset) performs no checks.
/// `Audit` computes the deny-by-default decision and LOGS would-be denials to stderr but
/// allows them. `Enforce` denies an ungranted access with a `HelixError`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Audit,
    Enforce,
}

/// The process authority: the mode plus the coarse per-category grants (phase 1 is on/off;
/// scoped paths/hosts arrive with cap-std). In `Audit`/`Enforce` an absent grant is denied.
#[derive(Clone, Debug)]
pub struct Authority {
    pub mode: Mode,
    pub fs_read: bool,
    pub fs_write: bool,
    pub net: bool,
    pub process: bool,
    pub db_write: bool,
    pub env: bool,
}

impl Authority {
    /// `Off` with everything nominally allowed — the gate is a no-op. This is the default
    /// for an unconfigured process (and for library/test code that drives `call_builtin`
    /// directly without installing an authority).
    pub fn unconfined() -> Self {
        Authority {
            mode: Mode::Off,
            fs_read: true,
            fs_write: true,
            net: true,
            process: true,
            db_write: true,
            env: true,
        }
    }

    fn allows(&self, e: Effect) -> bool {
        match e {
            Effect::Pure => true,
            Effect::FsRead => self.fs_read,
            Effect::FsWrite => self.fs_write,
            Effect::Net => self.net,
            Effect::Process => self.process,
            // A write is also a network access; both grants must hold.
            Effect::DbWrite => self.net && self.db_write,
            Effect::Env => self.env,
        }
    }
}

static AUTHORITY: OnceLock<Authority> = OnceLock::new();

/// The `[capabilities]` ceiling declared by the project's manifest, once the project is
/// known. Separate from [`AUTHORITY`] because the two are learned at different moments: the
/// environment is readable at startup, the manifest only after the entry file is resolved.
///
/// Combining at read time rather than rewriting `AUTHORITY` keeps the install path
/// single-write. An authority that could be replaced mid-run is a thing an attacker would
/// look for, and there is no reason to have one.
static CEILING: OnceLock<crate::pkg::Capabilities> = OnceLock::new();

/// Install the declared ceiling (idempotent — first writer wins). Called once the entry
/// file's project is known.
pub fn install_ceiling(c: crate::pkg::Capabilities) {
    let _ = CEILING.set(c);
}

/// The ceiling the loaded project declared, if any — for `helix build`, which has to bake
/// it into the artifact. A bundle has no manifest to read at run time, so the declaration
/// travels inside it or it does not travel at all.
pub fn declared_ceiling() -> Option<crate::pkg::Capabilities> {
    CEILING.get().cloned()
}

/// Install the process authority (idempotent — first writer wins). Call once at startup.
pub fn install(a: Authority) {
    let _ = AUTHORITY.set(a);
}

/// The authority actually in force: the environment's, narrowed by the manifest's declared
/// ceiling.
///
/// THE THREE PROPERTIES LIVE HERE.
///
/// - **Present means enforced.** A manifest that declares `[capabilities]` turns the gate
///   on by itself. `HELIX_CAP` is not consulted for the mode in that case, and in
///   particular `audit` cannot be used to weaken a declared ceiling: audit ALLOWS the
///   access it logs, so honouring it here would let the environment widen authority by
///   spelling a mode.
/// - **The environment narrows, never widens.** A grant survives only if BOTH allow it.
///   `HELIX_ALLOW_FS=all` against `fs = "read"` gives read.
/// - **An UNSET variable is not a denial.** Without a manifest, an absent
///   `HELIX_ALLOW_FS` denies (that is `install_from_env`'s rule and it is unchanged). With
///   one, the declaration stands on its own — otherwise every deployment would have to
///   restate what the repository already says, which is the failure the table exists to
///   end.
///
/// With no manifest ceiling this is exactly the installed authority, so an existing program
/// sees no change at all.
pub fn current() -> Authority {
    let env = AUTHORITY.get().cloned().unwrap_or_else(Authority::unconfined);
    let Some(c) = CEILING.get() else { return env };
    // The environment only narrows where it was actually configured. `Mode::Off` means no
    // variable asked for anything, so the ceiling stands alone.
    let env_says = env.mode != Mode::Off;
    let narrow = |ceiling: bool, from_env: bool| ceiling && (!env_says || from_env);
    Authority {
        mode: Mode::Enforce,
        fs_read: narrow(c.fs_read(), env.fs_read),
        fs_write: narrow(c.fs_write(), env.fs_write),
        net: narrow(c.net_on(), env.net),
        process: narrow(c.process_on(), env.process),
        db_write: narrow(c.db_write(), env.db_write),
        env: false,
    }
}

/// Build the authority from the environment (phase 1 bootstrap; the manifest
/// `[capabilities]` block and `--allow-*` flags supersede this in phase 1b) and install it.
/// `HELIX_CAP=off|audit|enforce` (default `off`); when a mode is set,
/// `HELIX_ALLOW_FS=read|write|all`, `HELIX_ALLOW_NET=on|all` and
/// `HELIX_ALLOW_PROCESS=on|all` grant coarse categories (default deny).
///
/// **An unrecognised `HELIX_CAP` is REFUSED, not treated as `off`.** It used to fall into
/// a `_ => Mode::Off` arm, so `HELIX_CAP=enfroce` — a typo in a deployment script, a
/// Dockerfile, a systemd unit — silently disabled the sandbox and ran the program fully
/// authorised. That is a security control that FAILS OPEN on a misspelling, and it fails
/// open silently: the program works, so nothing ever prompts a second look. An empty value
/// is still `off`, because `HELIX_CAP=` is how a shell unsets a variable it inherited.
///
/// Returns the message to print when the environment is malformed; the caller exits. This
/// is configuration read once at startup, not user program input, so refusing is the
/// fail-closed answer rather than an ADR 0024 abort.
pub fn install_from_env() -> Result<(), String> {
    let raw = std::env::var("HELIX_CAP").ok();
    let mode = match raw.as_deref() {
        Some("audit") => Mode::Audit,
        Some("enforce") => Mode::Enforce,
        None | Some("off") | Some("") => Mode::Off,
        Some(other) => {
            return Err(format!(
                "error: HELIX_CAP is `{other}`, which is not a mode
                 help: the modes are `off`, `audit` and `enforce`. Refusing rather than                  defaulting to `off`, because a typo here would silently run unsandboxed.
"
            ))
        }
    };
    if mode == Mode::Off {
        install(Authority::unconfined());
        return Ok(());
    }
    // A GRANT THAT DOES NOT PARSE IS REFUSED, not silently denied. Denying is the safe
    // half — it fails closed — but it fails closed WITHOUT SAYING SO, and the shape that
    // makes that dangerous is already in the tree: ADR 0021 describes net authority as "a
    // host:port allowlist checked before the socket", which is the eventual design and not
    // what phase 1 parses. A reader who follows it and writes
    // `HELIX_ALLOW_NET=example.com:443` believes they granted network access, gets
    // "capability denied" from a program they authorised, and has nothing pointing at the
    // variable. Refusing at startup turns a confusing runtime denial into one sentence.
    let grant = |name: &str, allowed: &[&str]| -> Result<Option<String>, String> {
        match std::env::var(name).ok().as_deref() {
            // Absent, or emptied — `VAR=` is how a shell unsets one it inherited.
            None | Some("") => Ok(None),
            Some(v) if allowed.contains(&v) => Ok(Some(v.to_string())),
            Some(other) => Err(format!(
                "error: {name} is `{other}`, which is not a grant\n\
                 help: the values are {}. Phase 1 grants are coarse on/off; scoped paths \
                 and host:port arrive with cap-std (ADR 0021), so a path or a hostname \
                 here is refused rather than silently granting nothing.\n",
                allowed.iter().map(|a| format!("`{a}`")).collect::<Vec<_>>().join(" or ")
            )),
        }
    };
    let fs = grant("HELIX_ALLOW_FS", &["read", "write", "all"])?.unwrap_or_default();
    let on = |v: Option<String>| v.is_some();
    install(Authority {
        mode,
        fs_read: fs == "read" || fs == "all",
        fs_write: fs == "write" || fs == "all",
        net: on(grant("HELIX_ALLOW_NET", &["on", "all"])?),
        // PROCESS AUTHORITY IS GRANTABLE. It was hardcoded `false` here while
        // `unconfined()` set it `true`, so `run` (ADR 0037 D3, the only `Process`
        // builtin) was unconditionally denied under `audit` and `enforce` with no way to
        // allow it — turning the sandbox on broke every program that shells out, and the
        // only remedy was to turn it back off. A category you cannot grant is not a
        // sandbox, it is a wall.
        process: on(grant("HELIX_ALLOW_PROCESS", &["on", "all"])?),
        // A DATABASE WRITE IS ITS OWN GRANT (ADR 0047). `postgres_query` spends `net`; a
        // session that can write spends this as well, so `HELIX_ALLOW_NET=on` alone keeps a
        // program read-only against every database it can reach.
        db_write: on(grant("HELIX_ALLOW_DB", &["write", "all"])?),
        // `Effect::Env` is classified but no builtin carries it yet (ADR 0037 D2 leaves
        // `env` with zero builtins), so there is nothing to grant and no variable for it.
        // It gains one in the same change that gives `env` a builtin.
        env: false,
    });
    Ok(())
}

/// A human-readable target (first string argument) for the audit log, if any — e.g. the path
/// for `read_text`, the URL for `http_get`. `listen`'s port is an `Int`, so it has none.
fn first_str_target(args: &[Value]) -> String {
    for a in args {
        if let Value::Str(s) = a {
            return format!(" {s}");
        }
    }
    String::new()
}

/// The gate for **builtins**: consulted by `call_builtin` before dispatch.
pub fn gate(name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    gate_effect(effect_of(name), name, args, line, col)
}

/// The gate for **methods**: consulted by the shared method sinks (`export_method`,
/// `net_method`, and the String `write_to`/`append_to` arm) before the effect runs. Covers
/// the fs-write and net-egress surface the builtin gate does not see.
pub fn gate_method(name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    gate_effect(method_effect_of(name), name, args, line, col)
}

/// Shared decision for the builtin and method gates. `Pure` effects and `Off` mode are
/// no-ops; a granted access is silent. In `Audit` an ungranted access is logged (stderr) and
/// allowed; in `Enforce` it is a `HelixError`.
pub fn gate_effect(eff: Effect, name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
    if !eff.gated() {
        return Ok(());
    }
    let auth = current();
    if auth.mode == Mode::Off || auth.allows(eff) {
        return Ok(());
    }
    match auth.mode {
        Mode::Audit => {
            eprintln!(
                "helix: capability [audit] would deny {}: {}{}",
                eff.label(),
                name,
                first_str_target(args)
            );
            Ok(())
        }
        Mode::Enforce => {
            // A DECLARED CEILING MAKES THE ENVIRONMENT HINT WRONG.
            //
            // This hint once offered two ways to grant authority and neither was real:
            // `[capabilities]` was refused by the manifest parser (deliberately — see
            // `pkg::Manifest` — so writing it could not *look* like it restricted a program
            // while doing nothing), and no `--allow-*` flag has ever existed. The one
            // mechanism that worked went unmentioned, so a reader who turned the sandbox on
            // had no way forward: both roads were walls, and the reachable exit was to turn
            // the sandbox off — the one outcome this subsystem exists to avoid.
            //
            // Now `[capabilities]` works, and under a ceiling the environment cannot widen
            // anything. Pointing at `HELIX_ALLOW_FS=write` there sends the reader to a
            // variable that provably will not help: the identical dead end, reintroduced by
            // the fix for it. So when a ceiling is in force, the ceiling is the answer — and
            // it names rebuilding too, because a built artifact has no manifest beside it to
            // edit.
            if declared_ceiling().is_some() {
                return Err(HelixError::new(
                    format!(
                        "capability denied: `{name}` needs `{}` authority, which this \
                         program does not declare",
                        eff.label()
                    ),
                    line,
                    col,
                )
                .hint(
                    "this is a `[capabilities]` ceiling, so no environment variable can \
                     grant it. Add the authority to `[capabilities]` in `helix.toml` — and \
                     if this is a built artifact, rebuild it, because the ceiling is baked \
                     in at `helix build` time.",
                ));
            }
            Err(HelixError::new(
                format!(
                    "capability denied: `{name}` needs `{}` authority, which is not granted",
                    eff.label()
                ),
                line,
                col,
            )
            .hint(match eff {
                Effect::FsRead => "grant it for this run with `HELIX_ALLOW_FS=read` (or `all`).",
                Effect::FsWrite => "grant it for this run with `HELIX_ALLOW_FS=write` (or `all`).",
                Effect::Net => "grant it for this run with `HELIX_ALLOW_NET=on`.",
                Effect::Process => {
                    "grant it for this run with `HELIX_ALLOW_PROCESS=on` — but note a subprocess \
                     reaches whatever ITS permissions allow, including the filesystem and network \
                     you declined here (ADR 0037 D3)."
                }
                // No builtin carries `Env` yet, and `Pure` never reaches a denial.
                Effect::DbWrite => {
                    "grant it for this run with `HELIX_ALLOW_DB=write` (or `all`), together with \
                     `HELIX_ALLOW_NET=on` — the database is reached over the network, and a \
                     write is more than a network access."
                }
                Effect::Env | Effect::Pure => {
                    "this effect has no grant variable yet; it is classified but ungranted."
                }
            }))
        }
        Mode::Off => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_builtins_are_ungated() {
        assert!(!effect_of("sqrt").gated());
        assert!(!effect_of("print").gated());
        assert!(!effect_of("emit").gated());
        assert!(!effect_of("sleep").gated());
        assert!(!effect_of("aes_keygen").gated());
    }

    #[test]
    fn a_database_write_is_its_own_effect() {
        assert!(matches!(effect_of("postgres_execute"), Effect::DbWrite));
        assert!(matches!(effect_of("postgres_query"), Effect::Net));
        assert!(matches!(method_effect_of("execute"), Effect::DbWrite));
        assert_eq!(Effect::DbWrite.label(), "db-write");
        let mut a = Authority::unconfined();
        a.net = false;
        assert!(!a.allows(Effect::DbWrite), "a write is also a network access");
        let mut a = Authority::unconfined();
        a.db_write = false;
        assert!(!a.allows(Effect::DbWrite));
        assert!(a.allows(Effect::Net), "net alone still reads");
    }

    #[test]
    fn every_effectful_builtin_is_categorised() {
        // Drift guard: every `pure: false` builtin is either capability-gated (fs/net
        // authority) or in this known-harmless allowlist (console I/O / time / randomness /
        // assertions — effects, but not fs/net authority). A NEW effectful builtin that is
        // neither fails here, forcing the capability decision at review time instead of
        // silently shipping an ungated authority (ADR 0021). (`read_int` reads the process's
        // own stdin — a console effect, not an fs/net authority — so it sits here alongside
        // the output builtins; ctype's finer capability model additionally gates it on
        // `CAP_GETKEY`, see ctype ADR 0011.)
        let harmless: &[&str] = &[
            "print", "emit", "write", "elog", "read_int", "sleep", "clock_monotonic",
            // Reading a clock is an effect, not an fs/net authority — the same call
            // `clock_monotonic` already made. `now()` reads the wall clock instead of
            // process-elapsed time; neither touches a file or a socket.
            "now",
            "aes_keygen", "aes_encrypt", "ed25519_keygen", "assert", "assert_eq", "assert_close",
            // Inspects a `try` record and raises on a mismatch: control flow, no authority.
            "assert_error",
            // Raising an error is a control-flow effect, not an fs/net authority.
            "raise",
            // A fresh cookie jar is an empty in-memory value; it touches no network or
            // filesystem until it is threaded into a request, which IS gated (`Net`).
            "cookie_jar",
        ];
        for b in crate::registry::BUILTINS {
            if b.pure || effect_of(b.path).gated() {
                continue;
            }
            assert!(
                harmless.contains(&b.path),
                "effectful builtin `{}` is ungated and not known-harmless — categorise it in \
                 `capability::effect_of` (is it fs/net?) or justify it in the harmless allowlist",
                b.path
            );
        }
    }

    #[test]
    fn known_fs_net_builtins_stay_gated() {
        // A refactor must never silently un-gate a real authority builtin.
        for n in [
            "read_text", "read_csv", "read_json", "read_dir", "read_vcf", "read_bam",
            "file_exists", "remove_file", "mkdir", "listen", "http_get",
            "rename", "fsync", "sync_dir", "create_new", "file_size", "read_at",
            "write_at", "truncate", "remove_dir", "lock_file", "try_lock_file",
            "read_bytes", "read_bytes_at",
        ] {
            assert!(effect_of(n).gated(), "`{n}` must remain capability-gated (ADR 0021)");
        }
    }

    #[test]
    fn authority_bearing_methods_are_categorised() {
        assert_eq!(method_effect_of("write_to"), Effect::FsWrite);
        assert_eq!(method_effect_of("append_to"), Effect::FsWrite);
        assert_eq!(method_effect_of("write_csv"), Effect::FsWrite);
        assert_eq!(method_effect_of("write_parquet"), Effect::FsWrite);
        assert_eq!(method_effect_of("write_fasta"), Effect::FsWrite);
        assert_eq!(method_effect_of("respond"), Effect::Net);
        assert_eq!(method_effect_of("send"), Effect::Net);
        assert_eq!(method_effect_of("accept"), Effect::Net);
        // Pure methods (data verbs, request-record read, string export) stay ungated.
        assert!(!method_effect_of("map").gated());
        assert!(!method_effect_of("mean").gated());
        assert!(!method_effect_of("request").gated());
        assert!(!method_effect_of("to_html").gated());
    }

    #[test]
    fn authority_bearing_builtins_are_categorised() {
        assert_eq!(effect_of("read_text"), Effect::FsRead);
        assert_eq!(effect_of("read_vcf"), Effect::FsRead);
        assert_eq!(effect_of("file_exists"), Effect::FsRead);
        assert_eq!(effect_of("mkdir"), Effect::FsWrite);
        assert_eq!(effect_of("remove_file"), Effect::FsWrite);
        assert_eq!(effect_of("http_get"), Effect::Net);
        assert_eq!(effect_of("listen"), Effect::Net);
    }

    #[test]
    fn off_mode_allows_everything_silently() {
        let a = Authority::unconfined();
        assert_eq!(a.mode, Mode::Off);
        assert!(a.allows(Effect::FsRead) && a.allows(Effect::Net));
    }

    #[test]
    fn enforce_denies_ungranted_and_allows_granted() {
        let denied =
            Authority { mode: Mode::Enforce, fs_read: false, fs_write: false, net: false, process: false, db_write: false, env: false };
        assert!(!denied.allows(Effect::FsRead));
        let granted = Authority { fs_read: true, ..denied.clone() };
        assert!(granted.allows(Effect::FsRead));
        assert!(!granted.allows(Effect::Net));
    }
}
