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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package: Package,
    /// Declared dependencies, by the name they are imported under.
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
}

fn default_version() -> String {
    "0.1.0".to_string()
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
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| err(format!("invalid `helix.toml`: {e}")))
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
                let sha = dep.sha256.as_deref().ok_or_else(|| {
                    err(format!(
                        "dependency `{name}` is a `url` source but has no `sha256`"
                    ))
                    .hint("pin the tarball's sha256 — it is the integrity check (ADR 0010).")
                })?;
                let dir = materialize_url_dep(&name, url, sha)?;
                (dir, format!("url+{url}"), Some(sha.to_string()))
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
    if let Some(existing) = Lockfile::load(root)?
        && existing != lock
    {
        return Err(err(
            "the project's dependencies no longer match `helix.lock`".to_string(),
        )
        .hint("a dependency's source changed since `helix sync`; run `helix sync` to update."));
    }
    Ok(dirs)
}

/// A content hash of a package's source tree: the sha256 over every `.helix` file
/// (sorted by relative path), each contributing its path and bytes. Deterministic
/// across machines, so the lockfile detects any change to a dependency's source.
pub fn hash_tree(dir: &Path) -> Result<String, HelixError> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_helix_files(dir, &mut files)?;
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

fn collect_helix_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HelixError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| err(format!("could not read directory `{}`: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| err(format!("could not read a directory entry: {e}")))?;
        let path = entry.path();
        if path.is_dir() {
            collect_helix_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("helix") {
            out.push(path);
        }
    }
    Ok(())
}

// ---- Remote (`url`) dependency sources ----

/// Fetch (unless cached), verify, and unpack a remote tarball dependency; return the
/// package's root directory inside the content-addressed cache. The `sha256` is the
/// trust boundary (ADR 0010): the download is rejected unless its hash matches, and the
/// cache is keyed by that hash — so a present cache entry was provably verified, and
/// fetching is skipped forever after.
#[cfg(feature = "http")]
fn materialize_url_dep(name: &str, url: &str, sha256: &str) -> Result<PathBuf, HelixError> {
    let dest = cache_root()?.join(sha256);
    // Cached from a previous verified extraction? (The hash in the path is the proof.)
    if let Some(root) = package_root(&dest) {
        return Ok(root);
    }
    // A leftover partial extraction (e.g. an interrupted run) — start clean.
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }

    let bytes = crate::net::fetch_verified(url, sha256)
        .map_err(|e| err(format!("dependency `{name}`: {e}")))?;
    extract_tarball(&bytes, &dest).map_err(|e| {
        let _ = std::fs::remove_dir_all(&dest); // don't leave a half-unpacked cache entry
        err(format!("dependency `{name}`: could not unpack the tarball: {e}"))
    })?;
    package_root(&dest)
        .ok_or_else(|| err(format!("dependency `{name}`: the downloaded tarball is empty")))
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

/// Unpack a (optionally gzipped) tarball's bytes into `dest`. `tar`'s `unpack` refuses
/// entries that would escape the destination (absolute paths, `..`), so a malicious
/// archive cannot write outside the cache.
#[cfg(feature = "http")]
fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<(), String> {
    use std::io::Read;
    // gzip magic (1f 8b) → gunzip first; otherwise treat the bytes as a plain tar.
    let is_gz = bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b;
    let tar_bytes: Vec<u8> = if is_gz {
        let mut dec = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).map_err(|e| format!("gunzip failed: {e}"))?;
        out
    } else {
        bytes.to_vec()
    };
    std::fs::create_dir_all(dest).map_err(|e| format!("could not create the cache dir: {e}"))?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    archive.unpack(dest).map_err(|e| format!("{e}"))?;
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

// ---- CLI subcommands (`helix new`, `helix sync`) ----

/// `helix new <name>` — initialize a `helix.toml` in the current directory.
pub fn cli_new(name: &str) -> Result<(), HelixError> {
    let cwd = cwd()?;
    let path = cwd.join("helix.toml");
    if path.exists() {
        return Err(err("`helix.toml` already exists in this directory".to_string()));
    }
    let body = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n# Dependencies are local \
         packages for now: `name = {{ path = \"../somelib\" }}`.\n[dependencies]\n"
    );
    std::fs::write(&path, body).map_err(|e| err(format!("could not write helix.toml: {e}")))?;
    println!("Created helix.toml for package `{name}`.");
    Ok(())
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
    if unchanged {
        println!("helix.lock is up to date ({n} dependenc{}).", if n == 1 { "y" } else { "ies" });
    } else {
        println!("Locked {n} dependenc{} → helix.lock", if n == 1 { "y" } else { "ies" });
        for p in &lock.packages {
            println!("  {} ({}, {})", p.name, p.source, &p.sha256[..p.sha256.len().min(12)]);
        }
    }
    Ok(())
}

fn cwd() -> Result<PathBuf, HelixError> {
    std::env::current_dir().map_err(|e| err(format!("cannot read the current directory: {e}")))
}

#[cfg(test)]
mod tests {
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
}
