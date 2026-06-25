//! Verified HTTPS downloads — the trust boundary for managed runtimes, packages,
//! toolchains, and self-update (see ADR 0010). The rule: **every byte from the
//! network is untrusted until its SHA-256 matches an expected value pinned in code
//! or recorded in a lockfile.** TLS protects the transport, but cannot protect
//! against a compromised mirror or a corporate TLS-intercepting proxy — so the hash
//! check, not TLS, is the trust boundary.
//!
//! HTTPS-only, pure-Rust TLS (rustls via `ureq`) to keep the binary self-contained
//! (never OpenSSL). Gated on the `http` feature (default-on) — the networking
//! primitive shared by `http_get`, the package manager's remote sources, and managed
//! runtimes. `--no-default-features` yields a network-free binary (air-gapped).

#[cfg(feature = "http")]
mod imp {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    /// Fetch `url` over HTTPS and confirm its SHA-256 equals `expected` (hex) before
    /// returning the bytes. The hash check is the trust boundary (ADR 0010).
    pub fn fetch_verified(url: &str, expected_sha256: &str) -> Result<Vec<u8>, String> {
        let bytes = fetch(url)?;
        verify_sha256(&bytes, expected_sha256)?;
        Ok(bytes)
    }

    /// Raw HTTPS GET. HTTPS is required (a plaintext URL is refused). No integrity
    /// check here — callers must verify, or use `fetch_verified`.
    pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
        if !url.starts_with("https://") {
            return Err(format!("refusing a non-HTTPS download URL: {url}"));
        }
        let resp = ureq::get(url)
            .call()
            .map_err(|e| format!("download failed for {url}: {e}"))?;
        let mut buf = Vec::new();
        resp.into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("reading the download body failed: {e}"))?;
        Ok(buf)
    }

    /// Verify `bytes` hash to `expected` (hex-encoded SHA-256, case-insensitive).
    pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
        let mut h = Sha256::new();
        h.update(bytes);
        let got = hex(&h.finalize());
        if got.eq_ignore_ascii_case(expected.trim()) {
            Ok(())
        } else {
            Err(format!(
                "hash mismatch — refusing the download (expected {expected}, got {got})"
            ))
        }
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sha256_verification_is_the_gate() {
            // Known vector: SHA-256("abc").
            let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
            assert!(verify_sha256(b"abc", expected).is_ok());
            // Case-insensitive on the hex.
            assert!(verify_sha256(b"abc", &expected.to_uppercase()).is_ok());
            // A single-byte change is rejected — the whole point.
            assert!(verify_sha256(b"abd", expected).is_err());
        }

        #[test]
        fn non_https_is_refused() {
            assert!(fetch("http://example.com/x").is_err());
            assert!(fetch("ftp://example.com/x").is_err());
        }

        // A real network round-trip — ignored by default so the suite stays offline /
        // air-gapped-friendly. Run with: `cargo test -- --ignored`.
        #[test]
        #[ignore]
        fn real_download_verifies_and_rejects_tampering() {
            // Immutable release asset (python-build-standalone SHA256SUMS for a tag).
            let url = "https://raw.githubusercontent.com/astral-sh/python-build-standalone/20240814/README.md";
            let bytes = fetch(url).expect("download");
            assert!(!bytes.is_empty());
            // A wrong hash must be rejected.
            assert!(verify_sha256(&bytes, "00".repeat(32).as_str()).is_err());
        }
    }
}

#[cfg(feature = "http")]
pub use imp::*;
