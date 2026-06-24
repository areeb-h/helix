//! Managed Python runtimes (ADR 0008/0009) — download, verify, and extract a
//! relocatable python-build-standalone interpreter, uv-style, so interop "just
//! works" without a system-Python hunt.
//!
//! This is the first consumer of the verified-download layer ([`crate::net`],
//! ADR 0010): the interpreter's SHA-256 is checked **before** extraction — TLS is
//! not trusted as the integrity boundary — and extraction refuses path-traversal
//! (zip-slip). Behind the `managed` feature.
//!
//! v1 scope: download + verify + extract for the pinned linux-x86_64 build; other
//! platforms error clearly until pinned, and wiring the embedded interpreter (PyO3)
//! to *use* the managed Python is the next increment.

#[cfg(feature = "managed")]
mod imp {
    use crate::net;
    use std::path::{Path, PathBuf};

    /// A pinned build: the URL plus the SHA-256 that is the trust anchor (ADR 0010).
    struct Build {
        version: &'static str,
        url: &'static str,
        sha256: &'static str,
    }

    /// The python-build-standalone build for the current platform. v1 pins
    /// linux-x86_64; other platforms get a clear "not pinned yet" error rather than a
    /// silent guess. (A generated cross-platform manifest comes with the next increment.)
    fn current_build() -> Result<Build, String> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Ok(Build {
                version: "3.12.5",
                url: "https://github.com/astral-sh/python-build-standalone/releases/download/20240814/cpython-3.12.5+20240814-x86_64-unknown-linux-gnu-install_only.tar.gz",
                sha256: "3be3bd9b7bd11f4a71bcaf16813fe61f93812cdb814da3fb0d7298f1086752ef",
            })
        } else {
            Err("no managed Python is pinned for this platform yet (v1 pins linux-x86_64)".into())
        }
    }

    /// Where managed runtimes live: `$XDG_DATA_HOME/helix/python` (or
    /// `~/.local/share/helix/python`). ADR 0010 dir conventions; Windows/macOS
    /// equivalents come with the cross-platform manifest.
    fn data_dir() -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("helix")
            .join("python")
    }

    /// `helix python <subcommand>`.
    pub fn cli(args: &[String]) -> Result<(), String> {
        match args.first().map(|s| s.as_str()) {
            Some("install") => install(),
            Some("list") => list(),
            Some("dir") => {
                println!("{}", data_dir().display());
                Ok(())
            }
            Some(other) => Err(format!(
                "unknown `helix python` subcommand `{other}` (try install / list / dir)"
            )),
            None => {
                println!("helix python — manage CPython runtimes for interop");
                println!("  helix python install   download + verify + extract a Python");
                println!("  helix python list      list installed managed Pythons");
                println!("  helix python dir       show where they live");
                Ok(())
            }
        }
    }

    fn install() -> Result<(), String> {
        let b = current_build()?;
        let dest = data_dir().join(format!("cpython-{}", b.version));
        if dest.exists() {
            println!("managed Python {} already installed at {}", b.version, dest.display());
            return Ok(());
        }
        println!("downloading managed Python {} …", b.version);
        // Hash-verified before we trust a single byte (ADR 0010).
        let bytes = net::fetch_verified(b.url, b.sha256)?;
        println!("verified {} bytes (sha256 ok); extracting …", bytes.len());
        extract_tar_gz(&bytes, &dest)?;
        println!("installed managed Python {} → {}", b.version, dest.display());
        Ok(())
    }

    fn list() -> Result<(), String> {
        let dir = data_dir();
        if !dir.exists() {
            println!("no managed Python installed yet — try `helix python install`");
            return Ok(());
        }
        let mut any = false;
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            println!("{}", entry.file_name().to_string_lossy());
            any = true;
        }
        if !any {
            println!("no managed Python installed yet — try `helix python install`");
        }
        Ok(())
    }

    /// Extract a gzip-tar into `dest`. `tar::Archive::unpack` is **zip-slip-safe by
    /// design** — it refuses to write outside the destination (path-traversal / `..`
    /// / absolute paths) — and handles symlinks, hardlinks, and entry ordering
    /// correctly (which a naive per-entry loop does not). We extract into a sibling
    /// `.incomplete` dir and atomically rename, so an interrupted install never leaves
    /// a half-tree. (ADR 0010 threat table: zip-slip.)
    fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), String> {
        use flate2::read::GzDecoder;
        use tar::Archive;

        let tmp = dest.with_extension("incomplete");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;

        let mut archive = Archive::new(GzDecoder::new(bytes));
        archive.set_preserve_permissions(true);
        if let Err(e) = archive.unpack(&tmp) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("extracting the archive failed: {e}"));
        }
        std::fs::rename(&tmp, dest).map_err(|e| format!("finalize install: {e}"))?;
        Ok(())
    }
}

#[cfg(feature = "managed")]
pub use imp::cli;
