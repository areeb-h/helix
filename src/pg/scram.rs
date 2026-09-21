//! SCRAM-SHA-256 (RFC 5802, RFC 7677) — the authentication modern PostgreSQL requires — and
//! its channel-bound form, SCRAM-SHA-256-PLUS (RFC 5929's `tls-server-end-point`).
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
//! AND THE EXCHANGE IS BOUND TO THE TLS SESSION IT RUNS OVER ([`Binding`]). TLS proves the
//! other end holds a certificate some trusted authority issued for this name; SCRAM proves it
//! knows the password. Neither proves they are the SAME other end: a proxy holding a
//! mis-issued certificate for the name terminates the TLS session and relays the SCRAM
//! messages to the real server untouched, and both checks pass. Channel binding closes that:
//! the client signs a hash of the certificate IT SAW into the transcript, the server compares
//! it with the certificate IT SENT, and a relay in the middle — which showed the client some
//! other certificate — makes the proof fail. The gap is narrower here than in `libpq`, where
//! `sslmode=require` verifies nothing, but it is real, and it costs nothing: no round trip,
//! one hash.
//!
//! Every primitive comes from a crate already in the tree: `sha2`, `hmac`, `base64`, and
//! `OsRng` via `aes-gcm`. PBKDF2 is a loop over HMAC and is written out below rather than
//! pulling a crate for eleven lines.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

type HmacSha256 = Hmac<Sha256>;

/// How an exchange is bound to the TLS session under it — the GS2 header of RFC 5802.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// `n,,` — this client cannot bind: there is no TLS, the URL said not to, or the
    /// certificate's signature names no hash (`end_point_hash`).
    None,
    /// `y,,` — this client CAN bind and the server did not offer to. The server sees this,
    /// and if it did offer — if something between the two removed `-PLUS` from its list — it
    /// fails the exchange. That is what makes "the server did not offer it" safe to believe.
    Unoffered,
    /// `p=tls-server-end-point,,` — bound, to this hash of the server's certificate.
    EndPoint(Vec<u8>),
}

impl Binding {
    fn gs2(&self) -> &'static str {
        match self {
            Binding::None => "n,,",
            Binding::Unoffered => "y,,",
            Binding::EndPoint(_) => "p=tls-server-end-point,,",
        }
    }

    /// The SASL mechanism this binding is spoken under.
    pub fn mechanism(&self) -> &'static str {
        match self {
            Binding::EndPoint(_) => "SCRAM-SHA-256-PLUS",
            _ => "SCRAM-SHA-256",
        }
    }

    /// `c=`: the GS2 header and, when bound, the certificate hash — base64, in the final
    /// message, and so inside the transcript both sides sign.
    fn channel(&self) -> String {
        let mut raw = self.gs2().as_bytes().to_vec();
        if let Binding::EndPoint(hash) = self {
            raw.extend_from_slice(hash);
        }
        b64().encode(raw)
    }
}

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
    binding: Binding,
    /// Retained between steps to build the auth message the proof is computed over.
    client_first_bare: String,
    server_signature: Vec<u8>,
}

impl Scram {
    pub fn new(password: &str, binding: Binding) -> Scram {
        Scram {
            password: password.to_string(),
            client_nonce: nonce(),
            binding,
            client_first_bare: String::new(),
            server_signature: Vec::new(),
        }
    }

    /// `n,,n=,r=<nonce>` — the user name is EMPTY on purpose. PostgreSQL takes the user
    /// from the startup packet and ignores SCRAM's `n=`, and sending it twice would only
    /// create a second place for the two to disagree.
    pub fn client_first(&mut self) -> String {
        self.client_first_bare = format!("n=,r={}", self.client_nonce);
        format!("{}{}", self.binding.gs2(), self.client_first_bare)
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

        let final_without_proof = format!("c={},r={combined}", self.binding.channel());
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

/// One DER element: its tag, its contents, and what follows it. `None` for anything that is
/// not well-formed — every length is checked against what is there, because a certificate
/// is bytes a server chose.
fn tlv(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = der.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let len = rest.get(..n)?.iter().fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        (len, rest.get(n..)?)
    };
    Some((tag, rest.get(..len)?, rest.get(len..)?))
}

/// `tls-server-end-point` (RFC 5929 §4.1): the hash of the server's certificate, under the
/// hash function of the certificate's OWN signature algorithm — except that MD5 and SHA-1 are
/// replaced by SHA-256. `None` when that signature names no hash this client knows (Ed25519
/// has none at all), in which case the binding is not defined and the exchange goes unbound
/// (`Binding::None`) rather than guess at what the server will compute.
pub fn end_point_hash(certificate: &[u8]) -> Option<Vec<u8>> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let (0x30, body, _) = tlv(certificate)? else { return None };
    let (_, _tbs, rest) = tlv(body)?;
    let (0x30, algorithm, _) = tlv(rest)? else { return None };
    let (0x06, oid, params) = tlv(algorithm)? else { return None };

    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01]; // pkcs-1
    const ECDSA_SHA2: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03]; // ecdsa-with-SHA2
    const ECDSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01];
    const NIST_HASH: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02]; // nistAlgorithms hashAlgs
    const SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];

    #[derive(Clone, Copy)]
    enum Hash {
        S224,
        S256,
        S384,
        S512,
    }
    let nist = |oid: &[u8]| match oid.strip_prefix(NIST_HASH)? {
        [1] => Some(Hash::S256),
        [2] => Some(Hash::S384),
        [3] => Some(Hash::S512),
        [4] => Some(Hash::S224),
        _ => None,
    };
    let hash = if let Some(which) = oid.strip_prefix(RSA) {
        match which {
            [0x04] | [0x05] => Hash::S256, // md5WithRSA, sha1WithRSA: replaced
            [0x0b] => Hash::S256,
            [0x0c] => Hash::S384,
            [0x0d] => Hash::S512,
            [0x0e] => Hash::S224,
            // RSASSA-PSS names its hash in its parameters: SEQUENCE { [0] AlgorithmIdentifier … },
            // and when it says nothing the default is SHA-1 — replaced, like any SHA-1.
            [0x0a] => {
                let named = tlv(params)
                    .filter(|(tag, _, _)| *tag == 0x30)
                    .and_then(|(_, pss, _)| tlv(pss))
                    .filter(|(tag, _, _)| *tag == 0xa0)
                    .and_then(|(_, explicit, _)| tlv(explicit))
                    .and_then(|(_, hash_alg, _)| tlv(hash_alg))
                    .map(|(_, hash_oid, _)| hash_oid);
                match named {
                    None => Hash::S256,
                    Some(o) if o == SHA1 => Hash::S256,
                    Some(o) => nist(o)?,
                }
            }
            _ => return None,
        }
    } else if let Some(which) = oid.strip_prefix(ECDSA_SHA2) {
        match which {
            [1] => Hash::S224,
            [2] => Hash::S256,
            [3] => Hash::S384,
            [4] => Hash::S512,
            _ => return None,
        }
    } else if oid == ECDSA_SHA1 {
        Hash::S256
    } else {
        return None;
    };
    Some(match hash {
        Hash::S224 => Sha224::digest(certificate).to_vec(),
        Hash::S256 => Sha256::digest(certificate).to_vec(),
        Hash::S384 => Sha384::digest(certificate).to_vec(),
        Hash::S512 => Sha512::digest(certificate).to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exchange with the nonce and the user name a test vector fixes.
    fn fixed(password: &str, user: &str, nonce: &str, binding: Binding) -> (Scram, String) {
        let mut s = Scram::new(password, binding);
        s.client_nonce = nonce.to_string();
        let first = s.client_first();
        // The vectors name a user; this client never does (see `client_first`).
        s.client_first_bare = format!("n={user},r={nonce}");
        (s, first)
    }

    /// RFC 7677 §3 — the published SCRAM-SHA-256 exchange, to the byte: the proof this client
    /// computes, and the server signature it then insists on.
    #[test]
    fn the_rfc_7677_exchange_to_the_byte() {
        let (mut s, first) = fixed("pencil", "user", "rOprNGfwEbeRWgbNEkqO", Binding::None);
        assert_eq!(first, "n,,n=,r=rOprNGfwEbeRWgbNEkqO");
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        assert_eq!(
            s.client_final(server_first).unwrap(),
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        s.verify_server("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
        // One bit off is a server that does not know the password.
        let e = s.verify_server("v=7rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap_err();
        assert!(e.contains("failed to prove"), "{e}");
    }

    /// A server that answers a nonce that is not an extension of ours is answering someone else.
    #[test]
    fn a_nonce_that_does_not_extend_the_clients_is_refused() {
        let (mut s, _) = fixed("pencil", "user", "abc", Binding::None);
        let e = s.client_final("r=xyzSERVER,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err();
        assert!(e.contains("does not extend"), "{e}");
        let e = s.client_final("r=abcSERVER,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=0").unwrap_err();
        assert!(e.contains("0 SCRAM iterations"), "{e}");
        let e = s.client_final("m=ext,r=abcSERVER,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err();
        assert!(e.contains("extension"), "{e}");
    }

    /// The three GS2 headers, the mechanism each is spoken under, and `c=` — which for a bound
    /// exchange carries the certificate hash INSIDE the signed transcript, so a different hash
    /// is a different proof.
    #[test]
    fn the_binding_is_in_the_transcript() {
        let server_first = "r=noncenonceSERVER,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let mut proofs = Vec::new();
        for (binding, header, mechanism, channel) in [
            (Binding::None, "n,,", "SCRAM-SHA-256", "biws".to_string()),
            (Binding::Unoffered, "y,,", "SCRAM-SHA-256", "eSws".to_string()),
            (
                Binding::EndPoint(vec![1, 2, 3]),
                "p=tls-server-end-point,,",
                "SCRAM-SHA-256-PLUS",
                b64().encode(b"p=tls-server-end-point,,\x01\x02\x03"),
            ),
            (
                Binding::EndPoint(vec![1, 2, 4]),
                "p=tls-server-end-point,,",
                "SCRAM-SHA-256-PLUS",
                b64().encode(b"p=tls-server-end-point,,\x01\x02\x04"),
            ),
        ] {
            assert_eq!(binding.mechanism(), mechanism);
            let (mut s, first) = fixed("pencil", "user", "noncenonce", binding);
            assert_eq!(first, format!("{header}n=,r=noncenonce"));
            let last = s.client_final(server_first).unwrap();
            assert!(last.starts_with(&format!("c={channel},r=noncenonceSERVER,p=")), "{last}");
            proofs.push(last);
        }
        proofs.sort();
        proofs.dedup();
        assert_eq!(proofs.len(), 4, "four bindings, four different proofs");
    }

    fn der(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match body.len() {
            n if n < 0x80 => out.push(n as u8),
            n if n < 0x100 => out.extend_from_slice(&[0x81, n as u8]),
            n => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
        }
        out.extend_from_slice(body);
        out
    }

    /// A certificate in outline: a to-be-signed part, THIS signature algorithm, a signature.
    fn certificate(algorithm: &[u8]) -> Vec<u8> {
        let tbs = der(0x30, &[0x02, 0x01, 0x01]);
        let signature = der(0x03, &[0u8; 65]);
        der(0x30, &[tbs, der(0x30, algorithm), signature].concat())
    }

    /// The hash is the one the certificate's own signature names — MD5 and SHA-1 replaced by
    /// SHA-256, RSASSA-PSS read from its parameters — and there is NONE for a signature that
    /// names no hash, which is what sends such a certificate's exchange out unbound.
    #[test]
    fn the_certificate_is_hashed_as_its_own_signature_says() {
        let oid = |bytes: &[u8]| der(0x06, bytes);
        let null = [0x05, 0x00];
        let rsa = |last: u8| [oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, last]), null.to_vec()].concat();
        let ecdsa = |last: u8| oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, last]);
        let pss = |hash: Option<&[u8]>| {
            let params = match hash {
                Some(h) => der(0x30, &der(0xa0, &der(0x30, &[oid(h), null.to_vec()].concat()))),
                None => der(0x30, &[]),
            };
            [oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a]), params].concat()
        };
        let sha384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
        let sha1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
        for (what, algorithm, bytes) in [
            ("sha256WithRSA", rsa(0x0b), 32),
            ("sha384WithRSA", rsa(0x0c), 48),
            ("sha512WithRSA", rsa(0x0d), 64),
            ("sha224WithRSA", rsa(0x0e), 28),
            ("sha1WithRSA, replaced", rsa(0x05), 32),
            ("md5WithRSA, replaced", rsa(0x04), 32),
            ("ecdsa-with-SHA256", ecdsa(2), 32),
            ("ecdsa-with-SHA384", ecdsa(3), 48),
            ("ecdsa-with-SHA512", ecdsa(4), 64),
            ("ecdsa-with-SHA1, replaced", oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01]), 32),
            ("RSASSA-PSS naming SHA-384", pss(Some(sha384)), 48),
            ("RSASSA-PSS naming SHA-1, replaced", pss(Some(sha1)), 32),
            ("RSASSA-PSS naming nothing: SHA-1, replaced", pss(None), 32),
        ] {
            let cert = certificate(&algorithm);
            let hash = end_point_hash(&cert).unwrap_or_else(|| panic!("{what}: no hash"));
            assert_eq!(hash.len(), bytes, "{what}");
            if bytes == 32 {
                assert_eq!(hash, Sha256::digest(&cert).to_vec(), "{what}: the hash is of the whole certificate");
            }
        }
        // No hash is named, so no binding is defined: Ed25519, and an algorithm nobody knows.
        assert_eq!(end_point_hash(&certificate(&oid(&[0x2b, 0x65, 0x70]))), None);
        assert_eq!(end_point_hash(&certificate(&oid(&[0x2a, 0x03, 0x04]))), None);
    }

    /// A certificate is bytes a server chose: cut anywhere, or lying about its lengths, it is
    /// `None` — never a panic, and never a hash of something that is not a certificate.
    #[test]
    fn a_certificate_that_is_not_one_has_no_hash() {
        let good = certificate(&[der(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]), vec![0x05, 0x00]].concat());
        assert!(end_point_hash(&good).is_some());
        for cut in 0..good.len() {
            assert_eq!(end_point_hash(&good[..cut]), None, "cut at {cut}");
        }
        for lie in [vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff, 0x00], vec![0x30, 0x80], vec![0x30, 0x85, 1, 1, 1, 1, 1], vec![]] {
            assert_eq!(end_point_hash(&lie), None, "{lie:?}");
        }
    }
}
