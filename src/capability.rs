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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    Pure,
    FsRead,
    FsWrite,
    Net,
    // Reserved: Helix has no subprocess or env-var builtin yet, so `effect_of` never returns
    // these — but the category set is part of the design (ADR 0021), and the `Authority` /
    // gate already handle them, so a future `spawn`/`getenv` is one `effect_of` arm away.
    #[allow(dead_code)]
    Process,
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
        "read_csv" | "read_parquet" | "read_text" | "read_json" | "read_dir" | "file_exists"
        | "read_fasta" | "read_fastq" | "read_vcf" | "read_bcf" | "read_sam" | "read_bam"
        | "read_gff" | "read_bed" => Effect::FsRead,
        "remove_file" | "mkdir" => Effect::FsWrite,
        "listen" | "http_get" | "http_post" | "http_request" | "http_stream" => Effect::Net,
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
        "accept" | "poll" | "respond" | "sse" | "send" => Effect::Net,
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
            Effect::Env => self.env,
        }
    }
}

static AUTHORITY: OnceLock<Authority> = OnceLock::new();

/// Install the process authority (idempotent — first writer wins). Call once at startup.
pub fn install(a: Authority) {
    let _ = AUTHORITY.set(a);
}

/// The installed authority, or `unconfined` (Off) if none was installed — so library/test
/// code that drives `call_builtin` directly is never gated.
pub fn current() -> Authority {
    AUTHORITY.get().cloned().unwrap_or_else(Authority::unconfined)
}

/// Build the authority from the environment (phase 1 bootstrap; the manifest `[capabilities]`
/// block and `--allow-*` flags supersede this in phase 1b) and install it. `HELIX_CAP=
/// off|audit|enforce` (default `off`); when a mode is set, `HELIX_ALLOW_FS=read|write|all`
/// and `HELIX_ALLOW_NET=on` grant coarse categories (default deny).
pub fn install_from_env() {
    let mode = match std::env::var("HELIX_CAP").ok().as_deref() {
        Some("audit") => Mode::Audit,
        Some("enforce") => Mode::Enforce,
        _ => Mode::Off,
    };
    if mode == Mode::Off {
        install(Authority::unconfined());
        return;
    }
    let fs = std::env::var("HELIX_ALLOW_FS").ok().unwrap_or_default();
    let net = matches!(std::env::var("HELIX_ALLOW_NET").ok().as_deref(), Some("on") | Some("all"));
    install(Authority {
        mode,
        fs_read: fs == "read" || fs == "all",
        fs_write: fs == "write" || fs == "all",
        net,
        process: false,
        env: false,
    });
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
fn gate_effect(eff: Effect, name: &str, args: &[Value], line: usize, col: usize) -> Result<(), HelixError> {
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
        Mode::Enforce => Err(HelixError::new(
            format!("capability denied: `{name}` needs `{}` authority, which is not granted", eff.label()),
            line,
            col,
        )
        .hint("grant it in `[capabilities]` in helix.toml, or run with the matching `--allow-…` (ADR 0021)")),
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
    fn every_effectful_builtin_is_categorised() {
        // Drift guard: every `pure: false` builtin is either capability-gated (fs/net
        // authority) or in this known-harmless allowlist (output / time / randomness /
        // assertions — effects, but not authority). A NEW effectful builtin that is neither
        // fails here, forcing the capability decision at review time instead of silently
        // shipping an ungated authority (ADR 0021).
        let harmless: &[&str] = &[
            "print", "emit", "write", "elog", "sleep", "clock_monotonic", "aes_keygen",
            "aes_encrypt", "ed25519_keygen", "assert", "assert_eq", "assert_close",
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
            Authority { mode: Mode::Enforce, fs_read: false, fs_write: false, net: false, process: false, env: false };
        assert!(!denied.allows(Effect::FsRead));
        let granted = Authority { fs_read: true, ..denied.clone() };
        assert!(granted.allows(Effect::FsRead));
        assert!(!granted.allows(Effect::Net));
    }
}
