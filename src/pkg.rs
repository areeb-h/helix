//! The package manager: `helix.toml` (the hand-edited manifest) and `helix.lock`
//! (the machine-generated, reproducible pinned dependency graph).
//!
//! Design — synthesizing each incumbent's documented failure (ADR 0009):
//! - **Immutable, content-addressed.** The lockfile pins every dependency by a
//!   sha256 of its source tree; a changed source is a *different* package. This
//!   removes npm/PyPI's "a published version mutated or vanished" class entirely.
//! - **No install-time resolver.** Resolution runs at `helix add`; `helix sync` only
//!   fetches and *verifies* against the lockfile. Installs are therefore bit-identical
//!   forever — no pip/npm "resolved differently today" non-reproducibility.
//! - **No code runs on install.** Packages are pure Helix source; nothing executes on
//!   add/sync (no npm `postinstall` supply-chain hole).
//! - **The hash is the trust boundary** (ADR 0010), not the transport.
//!
//! Sources:
//! - **`path`** — a local Helix package (fully reproducible offline — the killer property
//!   for a science language: "run this study's code in 2030, get a bit-identical
//!   dependency tree"). Pinned by a hash of its source tree.
//! - **`url` + `sha256`** — a remote HTTPS tarball (`.tar.gz`). The download is rejected
//!   unless it matches the pinned `sha256` (the trust boundary — ADR 0010), then it is
//!   unpacked into a **content-addressed cache** keyed by that hash. A cached entry was
//!   provably verified; fetching is skipped forever after. Air-gapped builds
//!   (`--no-default-features`) reject `url` deps with a clean error and stay path-only.
//!
//! Git/registry sources are the documented next step; they only change the *fetch* step
//! — the lockfile already carries the hash for any source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::HelixError;

/// `helix.toml` — the hand-edited manifest.
///
/// `deny_unknown_fields` is a SAFETY property here, not tidiness. Serde silently discards
/// keys it does not know, and the capability gate documents a `[capabilities]` block as its
/// intended durable source of truth (see `src/capability.rs`) while phase 1b remains
/// unimplemented. Writing that block therefore looked like it restricted a program's
/// authority and did nothing at all — the worst shape a security control can have. An
/// unknown key is now a clear error naming the key, so an unimplemented or misspelled
/// section fails loudly instead of granting silent trust.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: Package,
    /// Declared dependencies, by the name they are imported under.
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    /// Present when this directory is a WORKSPACE ROOT: it is the module anchor for the
    /// packages it lists, which keep their own manifests and stay separately
    /// distributable. See [`Workspace`] and ADR 0040.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Workspace>,
}

/// A `[workspace]` table. Its whole job is to separate the two meanings `helix.toml` had
/// been carrying at once: "this directory is a distributable package" and "in-project
/// imports are anchored here".
///
/// A repo of several packages needs both and could have only one. `project_context` stops
/// at the NEAREST manifest walking up, so putting a manifest in each package — which is
/// what `helix add <name> --path <dir>` consumes — made each package its own module root:
/// `import ui.parse` written inside `ui/` resolved to `ui/ui/parse.helix`. Adding a
/// manifest at the repo root did not help, because the nested one still won. A field
/// report established that with a three-way table before either of us had the mechanism
/// right.
///
/// With a `[workspace]`, the root anchors and the members stay packages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// Member directories, relative to this manifest.
    ///
    /// EXACT PATHS, not globs. A glob that matches nothing would move the anchor with no
    /// diagnostic — the silent failure this table exists to end — so the general form
    /// waits for a rule about what an empty match means.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// One sentence saying what the package is — for humans and for tooling
    /// (`helix describe --project` and registries to come). Optional everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    /// An SPDX identifier (`MIT`, `Apache-2.0`, …). Not validated against the SPDX
    /// list — it is metadata, not a gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    /// The MINIMUM Helix toolchain this package needs, e.g. `">=0.2.1"` (a bare
    /// `"0.2.1"` means the same). **Enforced at manifest load** — an older binary
    /// refuses with one clear error naming both versions, instead of the sixty
    /// confusing parse errors a new-syntax file would otherwise produce. This is
    /// the #19 review's incident verbatim: three binaries on one machine, and
    /// `error: unexpected character` sent the user hunting for a typo in their own
    /// source when the true cause was "your binary is too old".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helix: Option<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Parse `MAJOR.MINOR.PATCH`, optionally carrying the `-dev` marker, into a comparable
/// tuple. Still deliberately strict — no build metadata, no partial versions, and exactly
/// ONE pre-release spelling. A version is an ordering claim, and claims that cannot be
/// compared are not versions; `-dev` is admitted precisely because it CAN be compared.
///
/// **What the marker is for.** `scripts/release.sh` bumps this crate's version at release
/// time, so between releases the tree carried the version it had just SHIPPED — a build
/// from `main` reported `0.6.0` and so did the released binary. A project needing
/// something that landed after the tag had no way to say so: `helix = ">=0.6.0"` is
/// satisfied by the very binary that lacks it, and the user found out at run time with
/// `` `now` is not a known function `` instead of at the manifest check that exists to
/// prevent exactly that. With the marker, the tree after v0.6.0 reads `0.6.1-dev` and the
/// manifest can say `">=0.6.1-dev"`.
///
/// The marker ranks BELOW the release it names — `0.6.0 < 0.6.1-dev < 0.6.1` — which is
/// the whole point, and is why this returns a fourth component rather than just stripping
/// the suffix: stripping alone would make a dev build compare EQUAL to the release it has
/// not become, so it would claim to satisfy `>=0.6.1`.
///
/// Arbitrary tags (`-alpha.2`, `-rc1`) stay refused: they order against each other only by
/// convention, and one unorderable spelling is one too many.
fn parse_semver(s: &str) -> Option<(u64, u64, u64, u8)> {
    // Rank: a release sorts ABOVE its own pre-release.
    let (triple, rank) = match s.strip_suffix("-dev") {
        Some(t) => (t, 0u8),
        None => (s, 1u8),
    };
    let mut it = triple.split('.');
    let (a, b, c) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let num = |p: &str| -> Option<u64> {
        (!p.is_empty() && p.len() <= 10 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())?
    };
    Some((num(a)?, num(b)?, num(c)?, rank))
}



/// A declared dependency's source: either a local `path`, or a remote `url` tarball
/// pinned by `sha256`. (Future: `git`+`rev`, or a registry shorthand — all pinned by
/// hash in the lockfile.) Exactly one of `path`/`url` must be set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    /// A path to a local Helix package, relative to the manifest's directory.
    pub path: Option<String>,
    /// A remote HTTPS `.tar.gz` URL. Requires `sha256` (the integrity pin).
    pub url: Option<String>,
    /// The sha256 (hex) of the tarball — mandatory for a `url` dep. It is the trust
    /// boundary (ADR 0010): the download is rejected unless its hash matches.
    pub sha256: Option<String>,
}

/// `helix.lock` — machine-generated; the reproducible, hash-pinned graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// Lockfile format version, so future changes are detectable.
    pub version: u32,
    /// One entry per resolved dependency, sorted by name (deterministic output).
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    /// Where the package came from, e.g. `path+../stats-lib`.
    pub source: String,
    /// sha256 of the package's source tree — the integrity pin.
    pub sha256: String,
}

/// The current lockfile format version.
const LOCK_VERSION: u32 = 1;

impl Manifest {
    /// Load `<dir>/helix.toml`, or `Ok(None)` if there is no manifest there.
    pub fn load(dir: &Path) -> Result<Option<Manifest>, HelixError> {
        let path = dir.join("helix.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err(format!("could not read `{}`: {e}", path.display()))),
        };
        let m: Manifest = toml::from_str(&text).map_err(|e| {
            let d = err(format!("invalid `helix.toml`: {e}"));
            // An unknown key is REFUSED, never ignored (see the type's own note), but the
            // refusal reads as "your manifest is malformed" when the real cause is often
            // "your manifest is newer than this binary". Say so, since the version is
            // right here and the reader has no other way to tell the two apart.
            if e.to_string().contains("unknown field") {
                return d.hint(format!(
                    "if that key comes from a newer Helix, this build is {}; check the \
                     project's `helix` requirement. Unknown keys are refused rather than \
                     ignored, because a silently discarded section looks like it took \
                     effect.",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            d
        })?;
        m.validate(&path)?;
        Ok(Some(m))
    }

    /// The checks a parse cannot express: the version must be a comparable
    /// MAJOR.MINOR.PATCH, and a declared `helix` toolchain floor is ENFORCED here —
    /// at the one seam every consumer (`run`, `test`, `sync`, and every dependency's
    /// own manifest) already goes through.
    fn validate(&self, path: &Path) -> Result<(), HelixError> {
        // A PACKAGE NAME IS AN IMPORT PATH, so it must be an identifier -- checked here,
        // where the author can still fix it, rather than on the consumer's side where
        // `validate_dep_name` would refuse it and blame the wrong person.
        if !is_helix_identifier(&self.package.name) {
            return Err(err(format!(
                "`name` in `{}` must be a Helix identifier, got \"{}\"",
                path.display(),
                self.package.name
            ))
            .hint(
                "a package name is what an `import` writes: letters, digits and \
                 underscores, not starting with a digit. `my-package` cannot be imported.",
            ));
        }
        if parse_semver(&self.package.version).is_none() {
            return Err(err(format!(
                "`version` in `{}` must be MAJOR.MINOR.PATCH, optionally with the \
                 `-dev` marker (e.g. \"0.1.0\", \"0.2.0-dev\"), got \"{}\"",
                path.display(),
                self.package.version
            )));
        }
        if let Some(req) = &self.package.helix {
            let want = req.strip_prefix(">=").unwrap_or(req).trim();
            let Some(min) = parse_semver(want) else {
                return Err(err(format!(
                    "`helix` in `{}` must be a minimum version, `\">=X.Y.Z\"` or \
                     `\"X.Y.Z\"` (optionally `X.Y.Z-dev`), got \"{req}\"",
                    path.display()
                )));
            };
            // THE CRATE'S OWN VERSION IS A BUILD-TIME FACT, NOT USER INPUT. This was an
            // `.expect()`, which under `panic = "abort"` meant a malformed `version` in
            // THIS crate's Cargo.toml aborted the host mid-run — in every user's binary,
            // on every `helix run` inside a project that declares a floor. ADR 0024 says a
            // total runtime never aborts; an invariant about our own build belongs to the
            // gate (`the_crate_version_is_a_version`), not to a running program. Declining
            // to compare is the safe half: an unparseable own-version means the floor
            // simply is not enforced, rather than nothing running at all.
            let cur_str = env!("CARGO_PKG_VERSION");
            if parse_semver(cur_str).is_some_and(|cur| cur < min) {
                return Err(err(format!(
                    "this project requires Helix >= {want}, and this binary is {cur_str} \
                     (declared in `{}`)",
                    path.display()
                ))
                .hint(
                    "upgrade from https://github.com/areeb-h/helix/releases — one clear \
                     error here replaces the sixty confusing parse errors an old binary \
                     would produce on new syntax.",
                ));
            }
        }
        Ok(())
    }
}

impl Lockfile {
    /// Load `<dir>/helix.lock`, or `Ok(None)` if absent.
    pub fn load(dir: &Path) -> Result<Option<Lockfile>, HelixError> {
        let path = dir.join("helix.lock");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err(format!("could not read `{}`: {e}", path.display()))),
        };
        toml::from_str(&text).map(Some).map_err(|e| err(format!("invalid `helix.lock`: {e}")))
    }

    /// Write `<dir>/helix.lock` (deterministic; safe to commit to version control).
    pub fn write(&self, dir: &Path) -> Result<(), HelixError> {
        let path = dir.join("helix.lock");
        let body = toml::to_string_pretty(self)
            .map_err(|e| err(format!("could not serialize the lockfile: {e}")))?;
        let text = format!(
            "# This file is @generated by `helix sync`. Do not edit.\n# It pins every \
             dependency by content hash for reproducible builds.\n\n{body}"
        );
        std::fs::write(&path, text)
            .map_err(|e| err(format!("could not write `{}`: {e}", path.display())))
    }
}

/// Resolve a project's dependency graph from its manifest directory: walk every
/// (transitive) path dependency, hash each one, and return the lockfile plus a
/// `name -> source directory` map for the module loader. Deterministic and offline.
///
/// Cycles and duplicate names are reported as clean errors. v1 uses a single flat
/// dependency namespace (a name resolves to one package across the whole graph).
pub fn resolve(root_dir: &Path) -> Result<(Lockfile, BTreeMap<String, PathBuf>), HelixError> {
    let root = match Manifest::load(root_dir)? {
        Some(m) => m,
        None => return Ok((Lockfile { version: LOCK_VERSION, packages: Vec::new() }, BTreeMap::new())),
    };

    // name -> canonical directory; name -> source string for the lock.
    let mut dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    // A `url` dep's lock hash is the *tarball* sha256 (the download trust boundary),
    // not a tree hash — record it here so the final pass doesn't re-hash a remote.
    let mut pinned: BTreeMap<String, String> = BTreeMap::new();
    // Work list of (importer dir, dep name, dependency). The root's own deps first.
    let mut stack: Vec<(PathBuf, String, Dependency)> = root
        .dependencies
        .iter()
        .map(|(n, d)| (root_dir.to_path_buf(), n.clone(), d.clone()))
        .collect();

    while let Some((from_dir, name, dep)) = stack.pop() {
        // Resolve this dependency's source to (directory, lock source string, optional
        // pinned tarball hash) — branching on its kind. Exactly one of path/url.
        let (canon, source, pin) = match (&dep.path, &dep.url) {
            (Some(_), Some(_)) => {
                return Err(err(format!(
                    "dependency `{name}` has both `path` and `url`; choose one"
                )));
            }
            (Some(rel), None) => {
                let dir = from_dir.join(rel);
                let canon = dir.canonicalize().map_err(|e| {
                    err(format!("dependency `{name}` path `{rel}` not found: {e}"))
                })?;
                (canon, format!("path+{rel}"), None)
            }
            (None, Some(url)) => {
                let raw = dep.sha256.as_deref().ok_or_else(|| {
                    err(format!(
                        "dependency `{name}` is a `url` source but has no `sha256`"
                    ))
                    .hint("pin the tarball's sha256 — it is the integrity check (ADR 0010).")
                })?;
                // Validate/normalize the hash *before* it is ever used as a path
                // component or a trust value — an unchecked string like `../../x` would
                // otherwise escape the cache directory.
                let sha = normalize_sha256(raw)
                    .map_err(|m| err(format!("dependency `{name}`: {m}")))?;
                let dir = materialize_url_dep(&name, url, &sha)?;
                (dir, format!("url+{url}"), Some(sha))
            }
            (None, None) => {
                return Err(err(format!(
                    "dependency `{name}` has no source; add a `path` or a `url` + `sha256`"
                )));
            }
        };

        // A name must resolve to exactly one package across the graph (flat namespace).
        if let Some(existing) = dirs.get(&name) {
            if existing != &canon {
                return Err(err(format!(
                    "dependency name `{name}` resolves to two different packages — rename one"
                )));
            }
            continue; // already resolved (shared dependency)
        }
        dirs.insert(name.clone(), canon.clone());
        sources.insert(name.clone(), source);
        if let Some(p) = pin {
            pinned.insert(name.clone(), p);
        }

        // Recurse into this package's own dependencies, if it has a manifest.
        if let Some(sub) = Manifest::load(&canon)? {
            for (sub_name, sub_dep) in &sub.dependencies {
                stack.push((canon.clone(), sub_name.clone(), sub_dep.clone()));
            }
        }
    }

    // The lockfile hash: a path dep hashes its source tree; a url dep is already pinned
    // by its (verified) tarball hash. Sorted by name → deterministic output.
    let mut packages: Vec<LockedPackage> = Vec::with_capacity(dirs.len());
    for (name, dir) in &dirs {
        let sha256 = match pinned.get(name) {
            Some(s) => s.clone(),
            None => hash_tree(dir)?,
        };
        packages.push(LockedPackage { name: name.clone(), source: sources[name].clone(), sha256 });
    }
    Ok((Lockfile { version: LOCK_VERSION, packages }, dirs))
}

/// Resolve dependencies for **running** a project. Like [`resolve`], but if a
/// `helix.lock` exists the current resolution must match it exactly — a dependency
/// whose source has drifted since `helix sync` is a hard error, not a silent
/// difference. This makes reproducibility an enforced property: you cannot run with
/// dependencies that don't match the committed lockfile (stricter than Cargo, which
/// silently updates). No lockfile yet → resolve directly (a first run before `sync`).
pub fn resolve_for_run(root: &Path) -> Result<BTreeMap<String, PathBuf>, HelixError> {
    let (lock, dirs) = resolve(root)?;
    if let Some(existing) = Lockfile::load(root)? {
        let diffs = diff_lockfiles(&existing, &lock);
        if !diffs.is_empty() {
            let mut msg = String::from("the project's dependencies no longer match `helix.lock`:");
            for d in &diffs {
                msg.push_str(&format!("\n  - {d}"));
            }
            return Err(err(msg).hint("run `helix sync` to update the lockfile."));
        }
    }
    Ok(dirs)
}

/// Human-readable differences from the `stored` lockfile to a freshly-`current` one,
/// keyed by package name. An empty result means they match exactly. Used both to explain
/// a `helix run` failure and to drive `helix verify`.
fn diff_lockfiles(stored: &Lockfile, current: &Lockfile) -> Vec<String> {
    let index = |lock: &Lockfile| -> BTreeMap<String, LockedPackage> {
        lock.packages.iter().map(|p| (p.name.clone(), p.clone())).collect()
    };
    let (stored_by, current_by) = (index(stored), index(current));
    let mut out = Vec::new();
    for (name, c) in &current_by {
        match stored_by.get(name) {
            None => out.push(format!("`{name}` is not pinned in the lockfile")),
            Some(s) if s.source != c.source => {
                out.push(format!("`{name}` source changed ({} → {})", s.source, c.source))
            }
            Some(s) if s.sha256 != c.sha256 => {
                out.push(format!("`{name}` content changed since `helix sync` (hash differs)"))
            }
            Some(_) => {}
        }
    }
    for name in stored_by.keys() {
        if !current_by.contains_key(name) {
            out.push(format!("`{name}` is pinned in the lockfile but is no longer a dependency"));
        }
    }
    out
}

/// A content hash of a package's tree: the sha256 over every file (sorted by relative
/// path), each contributing its path and bytes. Deterministic across machines, so the
/// lockfile detects any change to a dependency.
///
/// EVERY file, not just `.helix`. Hashing only source left everything else a package
/// ships — a reference CSV, a FASTA, a lookup table — outside the lock, so swapping it
/// changed every answer the dependency gave while `helix verify` stayed green. ADR 0013
/// sells bit-identical reproducibility; a dependency is its content, not just its code.
///
/// Dot-entries are skipped. `.git` is the reason: pack filenames differ between clones of
/// the same commit, so hashing it would make the lock depend on how the tree was fetched
/// rather than on what is in it. The same exclusion covers editor and cache state.
/// Non-regular entries (symlinks, fifos, devices) are not hashed either — they are not
/// content, and the tarball unpacker already rejects them outright (see the threat model
/// below), so a published package cannot contain one.
pub fn hash_tree(dir: &Path) -> Result<String, HelixError> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_package_files(dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for f in &files {
        let rel = f.strip_prefix(dir).unwrap_or(f);
        // Path then length-prefixed bytes — unambiguous framing.
        hasher.update(rel.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        let bytes = std::fs::read(f)
            .map_err(|e| err(format!("could not read `{}`: {e}", f.display())))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_package_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HelixError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| err(format!("could not read directory `{}`: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| err(format!("could not read a directory entry: {e}")))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // `file_type` does NOT follow symlinks, which is what we want: a symlink is
        // neither a directory to descend into (it could point outside the package, or at
        // its own ancestor and loop forever) nor a file whose bytes are the package's.
        let ft = entry
            .file_type()
            .map_err(|e| err(format!("could not inspect `{}`: {e}", entry.path().display())))?;
        if ft.is_dir() {
            collect_package_files(&entry.path(), out)?;
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

// ---- Remote (`url`) dependency sources ----
//
// Threat model (these are the holes a remote-fetching package manager must close):
// - **Wrong/substituted bytes** → the pinned `sha256` is verified before anything is
//   written (ADR 0010); TLS is not the trust boundary, the hash is.
// - **Path-injection via the hash** → `normalize_sha256` enforces 64 hex chars before
//   it is ever used as a cache directory name.
// - **Decompression bomb** (a tiny gzip that inflates to terabytes) → the unpacker
//   *streams* (never decompresses fully into memory) and caps total bytes + entry count.
// - **Tar escape** (absolute paths, `..`, symlink/hardlink/device entries) → unsafe
//   paths are refused and only regular files and directories are extracted.
// - **Partial/raced cache** → extraction goes to a private temp dir, then is promoted
//   into place with an atomic rename, so a half-unpacked tree is never seen as cached.

/// The largest a dependency tarball may expand to. A source package is small; anything
/// approaching this is either misuse (ship data separately) or a decompression bomb.
#[cfg(feature = "http")]
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
/// A sanity cap on entry count (a source package with this many files is pathological).
#[cfg(feature = "http")]
const MAX_ENTRIES: usize = 100_000;

/// Validate and normalize a hex SHA-256: exactly 64 hex digits, lowercased. Rejects
/// anything else — crucially, before the value is used as a filesystem path component.
fn normalize_sha256(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(t.to_ascii_lowercase())
    } else {
        Err(format!("`{s}` is not a valid sha256 (expected 64 hexadecimal characters)"))
    }
}

/// Fetch (unless cached), verify, and unpack a remote tarball dependency; return the
/// package's root directory inside the content-addressed cache. `sha256` must already be
/// normalized (64-hex). The hash is the trust boundary: the download is rejected unless
/// it matches, and the cache is keyed by that hash — so a present entry was provably
/// verified, and fetching is skipped forever after.
#[cfg(feature = "http")]
fn materialize_url_dep(name: &str, url: &str, sha256: &str) -> Result<PathBuf, HelixError> {
    let cache = cache_root()?;
    // Cached from a previous verified extraction? (The hash in the path is the proof.)
    if let Some(root) = package_root(&cache.join(sha256)) {
        return Ok(root);
    }
    let bytes = crate::net::fetch_verified(url, sha256)
        .map_err(|e| err(format!("dependency `{name}`: {e}")))?;
    install_into_cache(&cache, sha256, &bytes)
        .map_err(|e| err(format!("dependency `{name}`: {}", e.message)))
}

/// Extract already-verified tarball `bytes` into the content-addressed cache under
/// `sha256` and return the package root. Unpacks into a private temp dir, then promotes
/// it with an atomic rename — so a crash or a concurrent `sync` never leaves a partial
/// tree that the cache-hit check would mistake for a complete package. Shared by the
/// resolve path (`materialize_url_dep`) and `helix add` (which seeds the cache from the
/// bytes it already downloaded to compute the hash).
#[cfg(feature = "http")]
fn install_into_cache(cache: &Path, sha256: &str, bytes: &[u8]) -> Result<PathBuf, HelixError> {
    let dest = cache.join(sha256);
    let tmp = cache.join(format!(".tmp-{sha256}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    extract_tarball(bytes, &tmp).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp);
        err(format!("could not unpack the tarball: {e}"))
    })?;
    match std::fs::rename(&tmp, &dest) {
        Ok(()) => {}
        // A concurrent sync already populated the cache — discard our copy and use it.
        Err(_) if dest.exists() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(err(format!("could not place the package in the cache: {e}")));
        }
    }
    package_root(&dest).ok_or_else(|| err("the downloaded tarball is empty".to_string()))
}

/// Hex SHA-256 of `bytes` — used by `helix add` to compute a `url` dependency's pin.
#[cfg(feature = "http")]
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Without networking (`--no-default-features`) a `url` dependency cannot be fetched —
/// fail with a clear, actionable error rather than silently degrading.
#[cfg(not(feature = "http"))]
fn materialize_url_dep(name: &str, _url: &str, _sha256: &str) -> Result<PathBuf, HelixError> {
    Err(err(format!(
        "dependency `{name}` is a `url` source, but this Helix binary was built without \
         networking (`--no-default-features`)"
    ))
    .hint("rebuild with the default features to fetch remote dependencies, or vendor the \
           package and depend on it by `path`."))
}

/// The package root inside an extraction directory: if it holds exactly one entry and
/// that entry is a directory (the universal `name-version/` tarball layout — GitHub
/// archives, npm's `package/`), that subdirectory is the root; otherwise the directory
/// itself. Returns `None` if the directory does not exist (i.e. nothing is cached yet).
#[cfg(feature = "http")]
fn package_root(dest: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dest)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    if entries.len() == 1 && entries[0].is_dir() {
        return Some(entries.remove(0));
    }
    Some(dest.to_path_buf())
}

/// Unpack a (optionally gzipped) tarball's bytes into `dest`, safely. The decompressor
/// is **streamed** (never fully buffered in memory) and the unpack is bounded and
/// validated entry-by-entry — see the threat model above. Refuses: paths escaping
/// `dest` (absolute / `..`), symlink/hardlink/device entries, and archives that exceed
/// the size or entry-count caps (decompression bombs).
#[cfg(feature = "http")]
fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<(), String> {
    use std::io::Read;
    // gzip magic (1f 8b) → wrap the bytes in a streaming gunzip; otherwise a plain tar.
    // Streaming (vs read-to-end) is what defuses a gzip bomb: we only ever pull as many
    // bytes as the size cap allows, never the full expanded image.
    let is_gz = bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b;
    let cursor = std::io::Cursor::new(bytes);
    let reader: Box<dyn Read> = if is_gz {
        Box::new(flate2::read::GzDecoder::new(cursor))
    } else {
        Box::new(cursor)
    };

    let mut archive = tar::Archive::new(reader);
    // A malicious archive must not be able to set perms (e.g. setuid) or write xattrs;
    // packages are plain source, so default permissions are correct.
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);
    archive.set_unpack_xattrs(false);

    std::fs::create_dir_all(dest).map_err(|e| format!("could not create the cache dir: {e}"))?;

    let mut total: u64 = 0;
    let mut count: usize = 0;
    let entries = archive.entries().map_err(|e| format!("reading the archive failed: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("reading an archive entry failed: {e}"))?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(format!("tarball has more than {MAX_ENTRIES} entries — refusing"));
        }

        // A package is plain files and directories. Refuse symlinks, hardlinks, and
        // device/fifo nodes outright — both a safety boundary (symlink-escape) and
        // because a package never legitimately needs them.
        let kind = entry.header().entry_type();
        if kind.is_symlink()
            || kind.is_hard_link()
            || kind.is_character_special()
            || kind.is_block_special()
            || kind.is_fifo()
        {
            let path = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
            return Err(format!(
                "tarball entry `{path}` is a {kind:?}; only regular files and directories \
                 are allowed in a package"
            ));
        }

        // Bound the total expansion (the header size is exact for tar; a file body is
        // exactly its declared length). We test *before* unpacking the body, so a bomb's
        // payload is never written or decompressed.
        total = total.saturating_add(entry.header().size().unwrap_or(0));
        if total > MAX_UNPACKED_BYTES {
            return Err(format!(
                "tarball expands to more than {} MiB — refusing (possible decompression bomb)",
                MAX_UNPACKED_BYTES / 1024 / 1024
            ));
        }

        // `unpack_in` performs the path-safety check, refusing absolute paths and any
        // `..` that would escape `dest`; it returns Ok(false) when it skips such an entry.
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|e| format!("unpacking an entry failed: {e}"))?;
        if !unpacked {
            return Err(
                "tarball contains an unsafe path (absolute, or `..` escaping the package) \
                 — refusing"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// The content-addressed download cache root. `HELIX_CACHE` overrides it (tests, CI,
/// reproducible sandboxes); otherwise XDG `$XDG_CACHE_HOME/helix/cache` (falling back to
/// `$HOME/.cache/helix/cache`) on unix, and `%LOCALAPPDATA%\helix\cache` on Windows.
#[cfg(feature = "http")]
fn cache_root() -> Result<PathBuf, HelixError> {
    if let Some(dir) = std::env::var_os("HELIX_CACHE").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    let base = base
        .ok_or_else(|| err("could not determine a cache directory; set HELIX_CACHE".to_string()))?;
    Ok(base.join("helix").join("cache"))
}

fn err(msg: String) -> HelixError {
    HelixError::new(msg, 0, 0)
}

// ---- CLI subcommands (`helix new`, `helix add`, `helix sync`) ----

/// `helix new <name>` — initialize a `helix.toml` in the current directory.
pub fn cli_new(name: &str) -> Result<(), HelixError> {
    let cwd = cwd()?;
    let path = cwd.join("helix.toml");
    if path.exists() {
        return Err(err("`helix.toml` already exists in this directory".to_string()));
    }
    // The template declares the toolchain floor from birth — the whole point of the
    // field, so a project created on 0.3 and opened by an 0.2 binary says "your binary is
    // too old" once instead of failing sixty ways.
    //
    // IT NAMES THE VERSION THAT WROTE IT, marker and all. An earlier draft derived a
    // lower floor so the manifest would be readable by older binaries, and that was the
    // bug: a project scaffolded by a `0.7.0-dev` binary declaring `>=0.6.0` invites a
    // 0.6.0 binary to open it and fail later, on whatever the author writes — the silent
    // wrong answer this project treats as the worst failure. The honest floor costs a
    // pre-marker binary a worse SENTENCE ("must be a minimum version" rather than "your
    // binary is too old"), and a loud imprecise error beats a quiet wrong one.
    let body = format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         description = \"\"\n\
         # authors = [\"Your Name <you@example.com>\"]\n\
         # license = \"MIT\"\n\
         # repository = \"https://github.com/you/{name}\"\n\
         # keywords = [\"…\"]\n\
         # The minimum Helix this package needs — enforced with one clear error.\n\
         helix = \">={cur}\"\n\
         \n\
         # Add dependencies with `helix add <name> --path ../lib` or `--url <tarball>`.\n\
         [dependencies]\n",
        cur = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(&path, body).map_err(|e| err(format!("could not write helix.toml: {e}")))?;
    let opts = crate::render::RenderOpts::auto();
    crate::report::Report::new("new", format!("created helix.toml for package `{name}`"))
        .field("package", name.to_string())
        .field("helix", format!(">= {}", env!("CARGO_PKG_VERSION")))
        .gap()
        .note(
            "next",
            "add a dependency",
            "`helix add <name> --path ../lib`, or `--url <tarball>` for a remote one",
        )
        .print(&opts);
    Ok(())
}

/// Where a `helix add`ed dependency comes from (parsed from the CLI flags).
pub enum AddSource {
    /// `--path <dir>` — a local package, relative to the manifest.
    Path(String),
    /// `--url <tarball>` with an optional `--sha256`. When the hash is omitted it is
    /// computed by fetching the tarball (you never hand-compute it); when given, it is
    /// verified against the real download.
    Url { url: String, sha256: Option<String> },
}

/// `n dependencies` / `1 dependency`. Four call sites spelled this inline, which is three
/// more chances to write `1 dependencies`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `helix add <name> --path <dir>` / `--url <tarball> [--sha256 <hash>]` — add (or
/// update) a dependency in `helix.toml` and re-lock.
pub fn cli_add(name: &str, source: AddSource) -> Result<(), HelixError> {
    let cwd = cwd()?;
    let (updated, n) = add_dependency(&cwd, name, source)?;
    let opts = crate::render::RenderOpts::auto();
    crate::report::Report::new(
        if updated { "update" } else { "add" },
        format!("{} `{name}`", if updated { "updated" } else { "added" }),
    )
    .field("dependency", name.to_string())
    .field("locked", plural(n, "dependency", "dependencies"))
    .print(&opts);
    Ok(())
}

/// Add (or update) dependency `name` in `<root>/helix.toml` and re-lock. The manifest is
/// edited with a format-preserving TOML editor, so existing comments and layout survive
/// (a plain re-serialize would lose them). Resolution happens now (at add time), not at
/// install time — a url's hash is pinned immediately. Returns `(was_update, dep_count)`.
pub fn add_dependency(
    root: &Path,
    name: &str,
    source: AddSource,
) -> Result<(bool, usize), HelixError> {
    validate_dep_name(name)?;
    let manifest_path = root.join("helix.toml");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(err("no `helix.toml` in this directory".to_string())
                .hint("create one with `helix new <name>`."));
        }
        Err(e) => return Err(err(format!("could not read `helix.toml`: {e}"))),
    };

    // Build the dependency's inline table, resolving a url's hash *before* touching the
    // manifest (so a failed fetch never leaves the manifest half-edited).
    let mut item = toml_edit::InlineTable::new();
    match source {
        AddSource::Path(p) => {
            let target = root
                .join(&p)
                .canonicalize()
                .map_err(|e| err(format!("path `{p}` not found: {e}")))?;
            // THE KEY IS WHAT `import` RESOLVES THROUGH, so it must be what the target
            // calls itself. Nothing connected the two: `helix add ui --path ./web` wrote a
            // key `ui` pointing at a package named `web`, and `import ui.x` then resolved
            // through the key -- so the program imported a package under a name its author
            // never chose, with no error anywhere.
            //
            // A target with no manifest declares no name and so cannot disagree; that stays
            // allowed, because refusing it would break every loose-directory dependency for
            // a guarantee it was never able to give.
            if let Some(m) = Manifest::load(&target)?
                && m.package.name != name
            {
                return Err(err(format!(
                    "`{}` calls itself `{}`, but it is being added as `{name}`",
                    target.display(),
                    m.package.name
                ))
                .hint(format!(
                    "an import resolves through the key, so `import {name}.x` would load \
                     `{}`. Add it as `helix add {} --path {p}`, or rename the package.",
                    m.package.name, m.package.name
                )));
            }
            item.insert("path", p.into());
        }
        AddSource::Url { url, sha256 } => {
            if !url.starts_with("https://") {
                return Err(err(format!("a url dependency must be https://, got `{url}`")));
            }
            let sha = prepare_url_dep(&url, sha256.as_deref())?;
            item.insert("url", url.into());
            item.insert("sha256", sha.into());
        }
    }

    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| err(format!("invalid `helix.toml`: {e}")))?;
    let deps = doc
        .entry("dependencies")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = deps
        .as_table_mut()
        .ok_or_else(|| err("`[dependencies]` in helix.toml is not a table".to_string()))?;
    let updated = table.contains_key(name);
    table.insert(name, toml_edit::Item::Value(toml_edit::Value::InlineTable(item)));
    std::fs::write(&manifest_path, doc.to_string())
        .map_err(|e| err(format!("could not write helix.toml: {e}")))?;

    // Re-lock so helix.lock reflects the new graph (and fetches/verifies the new dep).
    let (lock, dirs) = resolve(root)?;
    lock.write(root)?;
    Ok((updated, dirs.len()))
}

/// Fetch a url dependency to determine its pinned hash for `helix add`: compute the
/// sha256 (or verify a user-provided one against reality), and seed the cache from the
/// bytes we already have so the follow-up `resolve` is a cache hit, not a second
/// download. Without networking this is a clean, actionable error.
#[cfg(feature = "http")]
fn prepare_url_dep(url: &str, expected: Option<&str>) -> Result<String, HelixError> {
    let bytes = crate::net::fetch(url).map_err(|e| err(format!("fetching `{url}` failed: {e}")))?;
    let sha = sha256_hex(&bytes);
    if let Some(exp) = expected {
        let exp = normalize_sha256(exp).map_err(err)?;
        if exp != sha {
            return Err(err(format!(
                "the downloaded tarball's sha256 is {sha}, not the --sha256 you gave ({exp})"
            )));
        }
    }
    let cache = cache_root()?;
    if package_root(&cache.join(&sha)).is_none() {
        install_into_cache(&cache, &sha, &bytes)?;
    }
    Ok(sha)
}

#[cfg(not(feature = "http"))]
fn prepare_url_dep(_url: &str, _expected: Option<&str>) -> Result<String, HelixError> {
    Err(err(
        "cannot add a `url` dependency: this Helix binary was built without networking \
         (`--no-default-features`)"
            .to_string(),
    )
    .hint("rebuild with the default features, or vendor the package and use `--path`."))
}

/// A dependency name must be a Helix identifier — it becomes the `import <name>` segment.
/// The rule a package name and a dependency key both have to satisfy: a Helix identifier,
/// because that is what an `import` writes.
///
/// Shared deliberately. It was applied to the CONSUMER's dependency key and not to the
/// package's own `name`, so a package could call itself `my-package`, publish happily, and
/// hand the error to whoever tried to depend on it -- the one person who could not fix it.
fn is_helix_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

fn validate_dep_name(name: &str) -> Result<(), HelixError> {
    if is_helix_identifier(name) {
        Ok(())
    } else {
        Err(err(format!("`{name}` is not a valid dependency name")).hint(
            "use a Helix identifier: letters, digits, and underscores, not starting with a digit.",
        ))
    }
}

/// `helix sync` — resolve the manifest's dependencies and (re)write `helix.lock`,
/// pinning each by content hash. Deterministic; the lockfile is meant to be committed.
pub fn cli_sync() -> Result<(), HelixError> {
    let cwd = cwd()?;
    if Manifest::load(&cwd)?.is_none() {
        return Err(err("no `helix.toml` in the current directory".to_string())
            .hint("create one with `helix new <name>`."));
    }
    let (lock, dirs) = resolve(&cwd)?;
    let unchanged = Lockfile::load(&cwd)?.is_some_and(|existing| existing == lock);
    lock.write(&cwd)?;
    let n = dirs.len();
    let opts = crate::render::RenderOpts::auto();
    if unchanged {
        crate::report::Report::new("sync", "helix.lock is up to date")
            .field("locked", plural(n, "dependency", "dependencies"))
            .print(&opts);
        return Ok(());
    }
    let mut r = crate::report::Report::new("sync", "wrote helix.lock")
        .field("locked", plural(n, "dependency", "dependencies"))
        .gap();
    for p in &lock.packages {
        // The hash is the POINT of a lockfile, so it is shown, truncated to the prefix a
        // human compares. The full value stays in the file, where a machine reads it.
        r = r.field_owned(
            p.name.clone(),
            format!("{}  {}", &p.sha256[..p.sha256.len().min(12)], p.source),
        );
    }
    r.print(&opts);
    Ok(())
}

/// `helix verify` — check, without building or running anything, that the project is in
/// a reproducible, locked state. The CI / pre-commit gate.
pub fn cli_verify() -> Result<(), HelixError> {
    let n = verify(&cwd()?)?;
    crate::report::Report::new("verify", "every dependency matches helix.lock")
        .field("verified", plural(n, "dependency", "dependencies"))
        .print(&crate::render::RenderOpts::auto());
    Ok(())
}

/// The body of `helix verify`: require a `helix.toml` and a `helix.lock`, re-resolve the
/// graph (path-dep trees re-hashed, url deps fetched/verified as needed), and confirm it
/// matches the lock exactly. Returns the dependency count on success, or an error naming
/// precisely which dependencies drifted. Stricter than `helix run`, which tolerates a
/// missing lock on a first run; `verify` requires one.
pub fn verify(root: &Path) -> Result<usize, HelixError> {
    if Manifest::load(root)?.is_none() {
        return Err(err("no `helix.toml` in this directory".to_string())
            .hint("create one with `helix new <name>`."));
    }
    let stored = Lockfile::load(root)?.ok_or_else(|| {
        err("no `helix.lock` to verify against".to_string())
            .hint("run `helix sync` to create the lockfile first.")
    })?;
    let (current, dirs) = resolve(root)?;
    let diffs = diff_lockfiles(&stored, &current);
    if diffs.is_empty() {
        Ok(dirs.len())
    } else {
        let mut msg = String::from("the project does not match `helix.lock`:");
        for d in &diffs {
            msg.push_str(&format!("\n  - {d}"));
        }
        Err(err(msg)
            .hint("run `helix sync` to update the lockfile, or restore the dependency to match."))
    }
}

fn cwd() -> Result<PathBuf, HelixError> {
    std::env::current_dir().map_err(|e| err(format!("cannot read the current directory: {e}")))
}

#[cfg(test)]
mod tests {

    /// THE ORDERING CLAIM, which is the entire reason a marker is admitted at all.
    ///
    /// A dev tree must sort ABOVE the release it descends from and BELOW the one it is
    /// becoming. Getting only the first half — by stripping the suffix and comparing the
    /// triple — would make a dev build compare EQUAL to a release it has not become, so
    /// it would claim to satisfy `>=0.6.1` while missing everything 0.6.1 will contain.
    #[test]
    fn a_dev_marker_orders_below_the_release_it_names() {
        let v = |s: &str| parse_semver(s).expect(s);
        assert!(v("0.6.0") < v("0.6.1-dev"), "a dev tree is newer than the release it followed");
        assert!(v("0.6.1-dev") < v("0.6.1"), "and older than the one it is becoming");
        assert!(v("0.6.1") < v("0.7.0-dev"));
        assert!(v("0.6.1-dev") < v("0.7.0-dev"));
        // ONE spelling, or none. `-alpha.2` and `-rc1` order against each other only by
        // convention, and a version that cannot be compared is not a version.
        assert_eq!(parse_semver("0.6.1-alpha"), None);
        assert_eq!(parse_semver("0.6.1-rc1"), None);
        assert_eq!(parse_semver("0.6.1+build"), None);
        assert_eq!(parse_semver("-dev"), None);
        assert_eq!(parse_semver("1.0"), None);
        assert_eq!(parse_semver("1.0.0.0"), None);
        // The overflow guard the original had, still in place behind the new suffix strip.
        assert_eq!(parse_semver("1.0.99999999999-dev"), None);
    }

    /// The invariant that used to live as an `.expect()` INSIDE EVERY USER'S BINARY
    /// (`Manifest::validate`), where under `panic = "abort"` it would have aborted a
    /// running program to report a mistake in this crate's own Cargo.toml. It is a
    /// build-time fact, so it belongs to the gate. ADR 0024.
    #[test]
    fn the_crate_version_is_a_version() {
        assert!(
            parse_semver(env!("CARGO_PKG_VERSION")).is_some(),
            "this crate's own version is unparseable: {}",
            env!("CARGO_PKG_VERSION")
        );
    }

    /// THE ROUND TRIP `helix new` MUST SATISFY: the floor it writes has to be one this
    /// very binary accepts, or it emits a project it cannot then open.
    ///
    /// An earlier draft derived a LOWER floor from a `-dev` marker so older binaries could
    /// read the manifest. It computed `0.7.0` from `0.7.0-dev` — a version that has not
    /// shipped and that the writing binary does not satisfy — because it assumed markers
    /// are always patch markers. This asserts the property directly instead of a formula.
    #[test]
    fn a_scaffold_declares_a_floor_this_binary_satisfies() {
        let me = parse_semver(env!("CARGO_PKG_VERSION")).expect("own version parses");
        let floor = parse_semver(env!("CARGO_PKG_VERSION")).expect("the scaffold's floor parses");
        assert!(floor <= me, "helix new must not demand a version this binary lacks");
    }
    use super::*;

    #[test]
    fn manifest_round_trips() {
        let src = r#"
            [package]
            name = "demo"
            version = "0.2.0"

            [dependencies]
            stats = { path = "../stats" }
        "#;
        let m: Manifest = toml::from_str(src).unwrap();
        assert_eq!(m.package.name, "demo");
        assert_eq!(m.package.version, "0.2.0");
        assert_eq!(m.dependencies["stats"].path.as_deref(), Some("../stats"));
    }

    #[test]
    fn version_defaults_when_omitted() {
        let m: Manifest = toml::from_str("[package]\nname = \"x\"\n").unwrap();
        assert_eq!(m.package.version, "0.1.0");
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn resolve_path_dependency_and_hashes_it() {
        // Build a tiny two-package project in a temp dir and resolve it.
        let base = std::env::temp_dir().join(format!("helix_pkgtest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dep")).unwrap();
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x + 1\n").unwrap();
        std::fs::write(
            base.join("app/helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();

        let (lock, dirs) = resolve(&base.join("app")).unwrap();
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "dep");
        assert_eq!(lock.packages[0].source, "path+../dep");
        assert_eq!(lock.packages[0].sha256.len(), 64); // sha256 hex digest
        assert!(dirs.contains_key("dep"));

        // The hash is stable, and changes when the source changes.
        let h1 = lock.packages[0].sha256.clone();
        let (lock2, _) = resolve(&base.join("app")).unwrap();
        assert_eq!(lock2.packages[0].sha256, h1, "hash must be deterministic");
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x + 2\n").unwrap();
        let (lock3, _) = resolve(&base.join("app")).unwrap();
        assert_ne!(lock3.packages[0].sha256, h1, "hash must track source changes");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The lock covers a package's DATA, not just its code. Hashing only `.helix` left
    /// everything else a package ships outside the lock, so a reference table could be
    /// swapped — changing every answer the dependency gives — while `helix verify`
    /// reported "all match". ADR 0013 sells bit-identical reproducibility; that claim is
    /// only worth anything if it covers what the dependency actually reads.
    #[test]
    fn the_lock_hash_covers_every_file_a_package_ships() {
        let base = std::env::temp_dir().join(format!("helix_pkgdata_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dep/data")).unwrap();
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x\n").unwrap();
        std::fs::write(base.join("dep/data/rates.csv"), "name,value\nbase,10\n").unwrap();
        std::fs::write(
            base.join("app/helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();

        let h1 = resolve(&base.join("app")).unwrap().0.packages[0].sha256.clone();
        // The DATA file changes; not one byte of code does.
        std::fs::write(base.join("dep/data/rates.csv"), "name,value\nbase,999999\n").unwrap();
        let h2 = resolve(&base.join("app")).unwrap().0.packages[0].sha256.clone();
        assert_ne!(h2, h1, "a swapped data file must change the lock hash");

        // A dot-entry does not: `.git` pack filenames differ between clones of the same
        // commit, so hashing it would pin how the tree was fetched rather than what is
        // in it — a lock that fails on a fresh clone protects nobody.
        std::fs::create_dir_all(base.join("dep/.git")).unwrap();
        std::fs::write(base.join("dep/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let h3 = resolve(&base.join("app")).unwrap().0.packages[0].sha256.clone();
        assert_eq!(h3, h2, "dot-entries must not affect the hash");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_resolution_enforces_the_lockfile() {
        let base = std::env::temp_dir().join(format!("helix_pkglock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dep")).unwrap();
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x\n").unwrap();
        let app = base.join("app");
        std::fs::write(
            app.join("helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();

        // No lock yet → a first run resolves fine.
        assert!(resolve_for_run(&app).is_ok());
        // `sync` writes the lock; the matching state still runs.
        let (lock, _) = resolve(&app).unwrap();
        lock.write(&app).unwrap();
        assert!(resolve_for_run(&app).is_ok());
        // The dependency's source drifts → running now errors (reproducibility enforced).
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x + 1\n").unwrap();
        assert!(resolve_for_run(&app).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(feature = "http")]
    #[test]
    fn extract_tarball_unpacks_a_gzipped_archive() {
        use std::io::Write;
        // Build a tiny .tar.gz in memory: pkg/lib.helix with known contents.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let data = b"fn f(x) = x + 1\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, "pkg/lib.helix", &data[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }

        let dest = std::env::temp_dir().join(format!("helix_untar_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        extract_tarball(&gz, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("pkg/lib.helix")).unwrap(),
            "fn f(x) = x + 1\n"
        );
        // A single top-level dir → package_root descends into it (the npm/GitHub layout).
        assert_eq!(package_root(&dest).unwrap(), dest.join("pkg"));

        let _ = std::fs::remove_dir_all(&dest);
    }

    // The full `url`-dependency resolve path, exercised offline by pre-seeding the
    // content-addressed cache (so the verified-download step is a cache hit, no network).
    #[cfg(feature = "http")]
    #[test]
    fn resolve_url_dependency_from_cache() {
        let base = std::env::temp_dir().join(format!("helix_pkgurl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cache = base.join("cache");
        // A url dep is keyed by its tarball sha256; here it's a fixed cache key (no fetch).
        let sha = "00".repeat(32);
        let pkg = cache.join(&sha).join("dep-1.0");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("lib.helix"), "fn f(x) = x\n").unwrap();

        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("helix.toml"),
            format!(
                "[package]\nname = \"app\"\n\n[dependencies]\n\
                 dep = {{ url = \"https://example.com/dep.tar.gz\", sha256 = \"{sha}\" }}\n"
            ),
        )
        .unwrap();

        // SAFETY: this is the only test that reads HELIX_CACHE, so no cross-test race.
        unsafe { std::env::set_var("HELIX_CACHE", &cache) };
        let (lock, dirs) = resolve(&app).unwrap();
        unsafe { std::env::remove_var("HELIX_CACHE") };

        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "dep");
        assert_eq!(lock.packages[0].source, "url+https://example.com/dep.tar.gz");
        // The lock pins the *tarball* hash, not a re-hash of the extracted tree.
        assert_eq!(lock.packages[0].sha256, sha);
        // The loader points at the descended package root inside the cache.
        assert_eq!(dirs["dep"], pkg);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sha256_validation_blocks_path_injection() {
        // 64 hex → accepted and lowercased; everything else refused.
        assert_eq!(normalize_sha256(&"A".repeat(64)).unwrap(), "a".repeat(64));
        assert!(normalize_sha256("not-hex-but-the-right-length-aaaaaaaaaaaaaaaaaaaaaaaaaa!").is_err());
        assert!(normalize_sha256(&"a".repeat(63)).is_err()); // too short
        assert!(normalize_sha256(&"a".repeat(65)).is_err()); // too long
        // The attack: a hash that is actually a path. Must be refused before it could
        // ever be used as a cache directory name.
        assert!(normalize_sha256("../../../../etc/cron.d/evil").is_err());

        // End-to-end: a url dep with a path-like sha is rejected during resolve, with no
        // fetch and no filesystem escape.
        let base = std::env::temp_dir().join(format!("helix_pkgbadsha_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\n\
             dep = { url = \"https://example.com/d.tar.gz\", sha256 = \"../../etc/x\" }\n",
        )
        .unwrap();
        let e = resolve(&base).unwrap_err();
        assert!(e.message.contains("sha256"), "got: {}", e.message);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(feature = "http")]
    #[test]
    fn extraction_refuses_symlink_entries() {
        // A symlink entry is a classic tar-escape primitive; a package never needs one.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name("/etc/passwd").unwrap();
            header.set_cksum();
            builder.append_data(&mut header, "innocent.helix", std::io::empty()).unwrap();
            builder.finish().unwrap();
        }
        let dest = std::env::temp_dir().join(format!("helix_symlink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let e = extract_tarball(&tar_buf, &dest).unwrap_err();
        assert!(e.contains("Symlink") || e.contains("only regular"), "got: {e}");
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[cfg(feature = "http")]
    #[test]
    fn extraction_refuses_a_decompression_bomb() {
        // A header that *declares* a body larger than the cap. The size is checked before
        // the body is read, so a real bomb's payload is never written or decompressed —
        // here we never even provide the body.
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(MAX_UNPACKED_BYTES + 1);
            header.set_cksum();
            builder.append_data(&mut header, "bomb.bin", std::io::empty()).unwrap();
        }
        let dest = std::env::temp_dir().join(format!("helix_bomb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let e = extract_tarball(&tar_buf, &dest).unwrap_err();
        assert!(e.contains("bomb") || e.contains("MiB"), "got: {e}");
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn url_dependency_requires_a_sha256() {
        let base = std::env::temp_dir().join(format!("helix_pkgnosha_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\n\
             dep = { url = \"https://example.com/dep.tar.gz\" }\n",
        )
        .unwrap();
        let e = resolve(&base).unwrap_err();
        assert!(e.message.contains("sha256"), "got: {}", e.message);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_path_dependency_preserves_comments_and_locks() {
        let base = std::env::temp_dir().join(format!("helix_pkgadd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dep")).unwrap();
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("dep/lib.helix"), "fn f(x) = x\n").unwrap();
        let app = base.join("app");
        // A manifest with a comment we must not destroy.
        std::fs::write(
            app.join("helix.toml"),
            "[package]\nname = \"app\"\n\n# keep this comment\n[dependencies]\n",
        )
        .unwrap();

        let (updated, n) =
            add_dependency(&app, "dep", AddSource::Path("../dep".to_string())).unwrap();
        assert!(!updated);
        assert_eq!(n, 1);

        let text = std::fs::read_to_string(app.join("helix.toml")).unwrap();
        assert!(text.contains("# keep this comment"), "comment was dropped:\n{text}");
        // It parses, and the dependency is now present and resolvable.
        let m = Manifest::load(&app).unwrap().unwrap();
        assert_eq!(m.dependencies["dep"].path.as_deref(), Some("../dep"));
        // And the lockfile was written.
        assert!(Lockfile::load(&app).unwrap().is_some());

        // Adding the same name again is an *update*, not a duplicate.
        let (updated2, n2) =
            add_dependency(&app, "dep", AddSource::Path("../dep".to_string())).unwrap();
        assert!(updated2);
        assert_eq!(n2, 1);
        assert_eq!(
            std::fs::read_to_string(app.join("helix.toml")).unwrap().matches("dep = {").count(),
            1,
            "update must not duplicate the entry"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn verify_passes_when_locked_and_names_the_drift_otherwise() {
        let base = std::env::temp_dir().join(format!("helix_pkgverify_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dep")).unwrap();
        std::fs::create_dir_all(base.join("app")).unwrap();
        std::fs::write(base.join("dep/dep.helix"), "fn f(x) = x\n").unwrap();
        let app = base.join("app");
        std::fs::write(
            app.join("helix.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .unwrap();

        // No lockfile yet → verify refuses (stricter than run).
        let e = verify(&app).unwrap_err();
        assert!(e.message.contains("helix.lock"), "got: {}", e.message);

        // Sync, then verify passes.
        let (lock, _) = resolve(&app).unwrap();
        lock.write(&app).unwrap();
        assert_eq!(verify(&app).unwrap(), 1);

        // The dependency's source drifts → verify fails and names it.
        std::fs::write(base.join("dep/dep.helix"), "fn f(x) = x + 1\n").unwrap();
        let e = verify(&app).unwrap_err();
        assert!(e.message.contains("`dep`"), "should name the dep: {}", e.message);
        assert!(e.message.contains("content changed"), "got: {}", e.message);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn diff_lockfiles_reports_each_kind_of_drift() {
        let pkg = |n: &str, src: &str, sha: &str| LockedPackage {
            name: n.into(),
            source: src.into(),
            sha256: sha.into(),
        };
        let stored = Lockfile {
            version: 1,
            packages: vec![pkg("keep", "path+../keep", "aa"), pkg("gone", "path+../gone", "bb")],
        };
        let current = Lockfile {
            version: 1,
            packages: vec![pkg("keep", "path+../keep", "cc"), pkg("new", "path+../new", "dd")],
        };
        let diffs = diff_lockfiles(&stored, &current);
        let joined = diffs.join("\n");
        assert!(joined.contains("`keep` content changed"), "{joined}");
        assert!(joined.contains("`new` is not pinned"), "{joined}");
        assert!(joined.contains("`gone` is pinned"), "{joined}");
        // A clean match yields no diffs.
        assert!(diff_lockfiles(&stored, &stored).is_empty());
    }

    #[test]
    fn add_rejects_invalid_dependency_names() {
        let base = std::env::temp_dir().join(format!("helix_pkgaddbad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("helix.toml"), "[package]\nname = \"app\"\n").unwrap();
        for bad in ["1bad", "has space", "a-b", "", "../x"] {
            assert!(
                add_dependency(&base, bad, AddSource::Path(".".to_string())).is_err(),
                "name `{bad}` should be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // Live `helix add --url` (no --sha256): fetch a real tarball, compute + pin its hash,
    // seed the cache, and lock. Ignored by default (network). `cargo test -- --ignored`.
    #[cfg(feature = "http")]
    #[test]
    #[ignore]
    fn add_url_dependency_computes_the_hash() {
        let base = std::env::temp_dir().join(format!("helix_pkgaddurl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: lone env writer in an ignored, serially-run test.
        unsafe { std::env::set_var("HELIX_CACHE", base.join("cache")) };
        std::fs::write(base.join("helix.toml"), "[package]\nname = \"app\"\n").unwrap();

        let url = "https://static.crates.io/crates/cfg-if/cfg-if-1.0.0.crate".to_string();
        let (_, n) =
            add_dependency(&base, "cfgif", AddSource::Url { url, sha256: None }).unwrap();
        assert_eq!(n, 1);
        let m = Manifest::load(&base).unwrap().unwrap();
        assert_eq!(m.dependencies["cfgif"].sha256.as_deref().unwrap().len(), 64);
        unsafe { std::env::remove_var("HELIX_CACHE") };
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn lockfile_round_trips() {
        let lock = Lockfile {
            version: 1,
            packages: vec![LockedPackage {
                name: "stats".into(),
                source: "path+../stats".into(),
                sha256: "abc123".into(),
            }],
        };
        let text = toml::to_string_pretty(&lock).unwrap();
        let back: Lockfile = toml::from_str(&text).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(back.packages.len(), 1);
        assert_eq!(back.packages[0].name, "stats");
        assert_eq!(back.packages[0].sha256, "abc123");
    }

    /// An unknown `helix.toml` key is a hard error, not a silent discard.
    ///
    /// This is a SECURITY property. `src/capability.rs` documents a `[capabilities]` block
    /// as the intended durable source of truth for a program's authority, while phase 1b
    /// remains unimplemented — so before `deny_unknown_fields`, writing that block looked
    /// like it restricted the program and did absolutely nothing. A control that appears to
    /// be enforcing and is not is worse than one that is plainly absent.
    #[test]
    fn an_unknown_manifest_key_is_rejected_rather_than_silently_dropped() {
        // the case that motivated it
        let e = toml::from_str::<Manifest>(
            "[package]\nname = \"demo\"\n\n[capabilities]\nfs = \"read\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("capabilities"), "the message must name the key; got: {e}");

        // a typo in a known key is caught by the same rule
        let e = toml::from_str::<Manifest>("[package]\nnaem = \"demo\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("naem"), "got: {e}");

        // ...and valid manifests are unaffected, with `version` still defaulting
        let m: Manifest = toml::from_str("[package]\nname = \"demo\"\n").unwrap();
        assert_eq!(m.package.name, "demo");
        assert_eq!(m.package.version, "0.1.0");
        let m: Manifest =
            toml::from_str("[package]\nname = \"demo\"\nversion = \"0.2.0\"\n").unwrap();
        assert_eq!(m.package.version, "0.2.0");
    }

}
