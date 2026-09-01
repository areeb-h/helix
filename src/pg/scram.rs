//! SCRAM-SHA-256 (RFC 5802, RFC 7677) — the authentication modern PostgreSQL requires.
//!
//! `password_encryption` has defaulted to `scram-sha-256` since PostgreSQL 14, so this is
//! not one option among several; it is the way a client authenticates to a current server.
//! MD5 is deliberately NOT implemented: it is deprecated upstream, and offering it would
//! mean a client that quietly downgrades when a server asks it to.
//!
//! THE PASSWORD NEVER CROSSES THE WIRE. SCRAM is a challenge-response: the client proves
//! it knows the password by signing a transcript both sides computed independently. That
//! is also why the server's final signature is VERIFIED here rather than ignored — without
//! that check the exchange authenticates the client to the server but not the server to
//! the client, which is precisely the half that matters when someone is in the middle.
//!
//! Every primitive comes from a crate already in the tree: `sha2`, `hmac`, `base64`, and
//! `OsRng` via `aes-gcm`. PBKDF2 is a loop over HMAC and is written out below rather than
//! pulling a crate for eleven lines.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The GS2 header for "no channel binding", and its base64, which the client-final
/// message carries verbatim as `c=`.
const GS2_HEADER: &str = "n,,";
const GS2_B64: &str = "biws";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn hmac(key: &[u8], data: &[u8]) -> Result<[u8; 32], String> {
    let mut m = HmacSha256::new_from_slice(key).map_err(|_| "HMAC key rejected".to_string())?;
    m.update(data);
    Ok(m.finalize().into_bytes().into())
}

/// PBKDF2-HMAC-SHA256, one 32-byte block — which is all SCRAM-SHA-256 needs.
fn pbkdf2(password: &[u8], salt: &[u8], rounds: u32) -> Result<[u8; 32], String> {
    let mut prev = Vec::with_capacity(salt.len() + 4);
    prev.extend_from_slice(salt);
    prev.extend_from_slice(&1u32.to_be_bytes()); // block index, always 1 for 32 bytes
    let mut u = hmac(password, &prev)?;
    let mut out = u;
    for _ in 1..rounds {
        u = hmac(password, &u)?;
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    Ok(out)
}

/// A client-side SCRAM exchange, carried across the three messages it takes.
pub struct Scram {
    password: String,
    client_nonce: String,
    /// Retained between steps to build the auth message the proof is computed over.
    client_first_bare: String,
    server_signature: Vec<u8>,
}

impl Scram {
    pub fn new(password: &str) -> Scram {
        Scram {
            password: password.to_string(),
            client_nonce: nonce(),
            client_first_bare: String::new(),
            server_signature: Vec::new(),
        }
    }

    /// `n,,n=,r=<nonce>` — the user name is EMPTY on purpose. PostgreSQL takes the user
    /// from the startup packet and ignores SCRAM's `n=`, and sending it twice would only
    /// create a second place for the two to disagree.
    pub fn client_first(&mut self) -> String {
        self.client_first_bare = format!("n=,r={}", self.client_nonce);
        format!("{GS2_HEADER}{}", self.client_first_bare)
    }

    /// Consume `r=<nonce>,s=<salt>,i=<rounds>` and produce the client's final message.
    pub fn client_final(&mut self, server_first: &str) -> Result<String, String> {
        let (mut nonce_s, mut salt_b64, mut rounds) = (None, None, None);
        for part in server_first.split(',') {
            match part.split_once('=') {
                Some(("r", v)) => nonce_s = Some(v),
                Some(("s", v)) => salt_b64 = Some(v),
                Some(("i", v)) => rounds = Some(v),
                // `m=` is a mandatory-extension marker: the RFC says a client that does
                // not understand it MUST fail rather than proceed.
                Some(("m", v)) => {
                    return Err(format!("the server requires SCRAM extension `{v}`, which this client does not implement"))
                }
                _ => {}
            }
        }
        let combined = nonce_s.ok_or("the server's SCRAM reply has no nonce")?;
        let salt = b64()
            .decode(salt_b64.ok_or("the server's SCRAM reply has no salt")?)
            .map_err(|_| "the server's SCRAM salt is not valid base64".to_string())?;
        let rounds: u32 = rounds
            .ok_or("the server's SCRAM reply has no iteration count")?
            .parse()
            .map_err(|_| "the server's SCRAM iteration count is not a number".to_string())?;
        if rounds == 0 {
            return Err("the server asked for 0 SCRAM iterations".to_string());
        }
        // THE SERVER MUST EXTEND OUR NONCE, NOT REPLACE IT. This is the client's
        // anti-replay check: a nonce that does not start with the one we just generated
        // means this is not a reply to our challenge.
        if !combined.starts_with(&self.client_nonce) {
            return Err("the server's SCRAM nonce does not extend the client's".to_string());
        }

        let salted = pbkdf2(self.password.as_bytes(), &salt, rounds)?;
        let client_key = hmac(&salted, b"Client Key")?;
        let stored_key: [u8; 32] = Sha256::digest(client_key).into();

        let final_without_proof = format!("c={GS2_B64},r={combined}");
        let auth_message =
            format!("{},{},{}", self.client_first_bare, server_first, final_without_proof);

        let client_sig = hmac(&stored_key, auth_message.as_bytes())?;
        let proof: Vec<u8> =
            client_key.iter().zip(client_sig.iter()).map(|(a, b)| a ^ b).collect();

        let server_key = hmac(&salted, b"Server Key")?;
        self.server_signature = hmac(&server_key, auth_message.as_bytes())?.to_vec();

        Ok(format!("{final_without_proof},p={}", b64().encode(proof)))
    }

    /// Verify `v=<signature>`. Failing this means the peer could not prove it knows the
    /// stored key — i.e. it is not the server it claims to be — so it is an error, not a
    /// warning.
    pub fn verify_server(&self, server_final: &str) -> Result<(), String> {
        let sig = server_final
            .split(',')
            .find_map(|p| p.strip_prefix("v="))
            .ok_or("the server's final SCRAM message has no signature")?;
        let got = b64()
            .decode(sig)
            .map_err(|_| "the server's SCRAM signature is not valid base64".to_string())?;
        // Length-independent compare, then constant-time over the bytes: this is a MAC
        // comparison, and an early exit leaks where the mismatch is.
        if got.len() != self.server_signature.len() {
            return Err("the server failed to prove it knows the password".to_string());
        }
        let mut diff = 0u8;
        for (a, b) in got.iter().zip(self.server_signature.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return Err("the server failed to prove it knows the password".to_string());
        }
        Ok(())
    }
}

/// A fresh client nonce: 18 random bytes as base64, which is printable and comfortably
/// above the RFC's minimum.
///
/// `OsRng` is the same source `aes-gcm` uses for its AEAD nonces here, so this adds no
/// dependency and no second opinion about where randomness comes from.
fn nonce() -> String {
    use aes_gcm::aead::{rand_core::RngCore, OsRng};
    let mut raw = [0u8; 18];
    OsRng.fill_bytes(&mut raw);
    b64().encode(raw)
}
