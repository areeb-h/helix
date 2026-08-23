//! Builtins: encodings, hashing, and cryptography — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

pub(super) fn call(name: &str, args: Vec<Value>, line: usize, col: usize) -> Called {
    if !matches!(name, "sha256" | "hmac_sha256" | "url_encode" | "url_decode" | "url_decode_lenient" | "base64_encode" | "base64_decode" | "hex_encode" | "hex_decode" | "aes_keygen" | "aes_encrypt" | "aes_decrypt" | "ed25519_keygen" | "ed25519_sign" | "ed25519_verify") {
        return Called::Not(args);
    }
    Called::Done(dispatch(name, args, line, col))
}

fn dispatch(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
                "sha256" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Str(s) => {
                            use sha2::{Digest, Sha256};
                            let mut hasher = Sha256::new();
                            hasher.update(s.as_bytes());
                            Ok(Value::Str(Rc::new(format!("{:x}", hasher.finalize()))))
                        }
                        other => Err(type_err("sha256", "a string", other, line, col)),
                    }
                }
                // HMAC-SHA256(key, message) → lowercase hex. The auth workhorse (JWT, webhook
                // and API-request signing, secure tokens) — needs raw-byte HMAC that pure Helix
                // can't do (its strings are UTF-8). Key/message are hashed by their UTF-8 bytes.
                "hmac_sha256" => {
                    arity(name, &args, 2, line, col)?;
                    if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                        return Ok(Value::Missing);
                    }
                    let key = match &args[0] {
                        Value::Str(s) => s,
                        other => return Err(type_err("hmac_sha256", "a string key", other, line, col)),
                    };
                    let msg = match &args[1] {
                        Value::Str(s) => s,
                        other => return Err(type_err("hmac_sha256", "a string message", other, line, col)),
                    };
                    use hmac::{Hmac, Mac};
                    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key.as_bytes())
                        .expect("HMAC accepts a key of any length");
                    mac.update(msg.as_bytes());
                    let tag = mac.finalize().into_bytes();
                    let hex: String = tag.iter().map(|b| format!("{:02x}", b)).collect();
                    Ok(Value::Str(Rc::new(hex)))
                }
                // A fresh, empty cookie jar (ADR 0031). Explicit state the program holds
                // and threads into `http_request({… , jar: jar})`.
                "url_encode" => {
                    if args.is_empty() || args.len() > 2 {
                        return Err(HelixError::new(
                            format!(
                                "`url_encode` takes a string and an optional set name, got {} arguments",
                                args.len()
                            ),
                            line,
                            col,
                        ));
                    }
                    // Which characters survive unescaped: the RFC 3986 grammar named by
                    // the second argument. The default stays the STRICT unreserved set —
                    // over-escaping is always safe — but a path segment, query, fragment
                    // and userinfo each legally carry more, and a hand-rolled encoder was
                    // the field's workaround for the sets this refused.
                    let extra: &[u8] = match args.get(1) {
                        None => b"",
                        Some(Value::Str(set)) => match set.as_str() {
                            "segment" => b"!$&'()*+,;=:@",
                            "query" | "fragment" => b"!$&'()*+,;=:@/?",
                            "userinfo" => b"!$&'()*+,;=:",
                            other => {
                                return Err(HelixError::new(
                                    format!("`url_encode` does not know the character set `{other}`"),
                                    line,
                                    col,
                                )
                                .hint(
                                    "sets: \"segment\", \"query\", \"fragment\", \"userinfo\" — \
                                     RFC 3986's grammars; omit the argument for the strict \
                                     unreserved-only set.",
                                ))
                            }
                        },
                        Some(other) => {
                            return Err(type_err("url_encode", "a set name (a string)", other, line, col))
                        }
                    };
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => {
                            let mut out = String::with_capacity(s.len());
                            for byte in s.as_bytes() {
                                match byte {
                                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_'
                                    | b'~' => out.push(*byte as char),
                                    other if extra.contains(other) => out.push(*other as char),
                                    other => out.push_str(&format!("%{:02X}", other)),
                                }
                            }
                            Ok(Value::Str(Rc::new(out)))
                        }
                        other => Err(type_err("url_encode", "a string", other, line, col)),
                    }
                }
                // The inverse. A `%` that is not followed by two hex digits is an error
                // rather than a pass-through: silently keeping it would turn a truncated
                // or mistyped escape into data, and the caller is usually parsing something
                // that arrived over a network.
                "url_decode" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => {
                            let bytes = s.as_bytes();
                            let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
                            let mut i = 0usize;
                            while i < bytes.len() {
                                if bytes[i] == b'%' {
                                    let hex = bytes.get(i + 1..i + 3).and_then(|h| {
                                        std::str::from_utf8(h)
                                            .ok()
                                            .and_then(|h| u8::from_str_radix(h, 16).ok())
                                    });
                                    match hex {
                                        Some(v) => {
                                            out.push(v);
                                            i += 3;
                                        }
                                        None => {
                                            return Err(HelixError::new(
                                                format!(
                                                    "`url_decode` found a `%` that is not followed by two hex digits, at position {i}"
                                                ),
                                                line,
                                                col,
                                            )
                                            .hint("a literal percent sign is written `%25`."))
                                        }
                                    }
                                } else {
                                    out.push(bytes[i]);
                                    i += 1;
                                }
                            }
                            match String::from_utf8(out) {
                                Ok(text) => Ok(Value::Str(Rc::new(text))),
                                Err(_) => Err(HelixError::new(
                                    "`url_decode` produced non-UTF-8 bytes — Helix strings are text, so binary payloads aren't representable",
                                    line,
                                    col,
                                )
                                .hint("decode binary with `base64_decode`, or keep it encoded.")),
                            }
                        }
                        other => Err(type_err("url_decode", "a string", other, line, col)),
                    }
                }
                // The lenient twin, for ATTACKER-CHOSEN input at a server edge, where
                // the strict error is a denial-of-service primitive: a malformed `%`
                // stays a literal `%`, and non-UTF-8 decodes with replacement
                // characters — this NEVER raises on any string. `url_decode` stays the
                // right call for trusted input, where a malformed escape is a bug
                // worth hearing about. (Both `http` and `web` in the field corpus had
                // hand-rolled exactly this, twice.)
                "url_decode_lenient" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => {
                            let bytes = s.as_bytes();
                            let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
                            let mut i = 0usize;
                            while i < bytes.len() {
                                if bytes[i] == b'%'
                                    && let Some(v) = bytes.get(i + 1..i + 3).and_then(|h| {
                                        std::str::from_utf8(h)
                                            .ok()
                                            .and_then(|h| u8::from_str_radix(h, 16).ok())
                                    })
                                {
                                    out.push(v);
                                    i += 3;
                                } else {
                                    out.push(bytes[i]);
                                    i += 1;
                                }
                            }
                            Ok(Value::Str(Rc::new(String::from_utf8_lossy(&out).into_owned())))
                        }
                        other => Err(type_err("url_decode_lenient", "a string", other, line, col)),
                    }
                }
                "base64_encode" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => {
                            use base64::Engine;
                            let enc = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
                            Ok(Value::Str(Rc::new(enc)))
                        }
                        other => Err(type_err("base64_encode", "a string", other, line, col)),
                    }
                }
                "base64_decode" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => {
                            use base64::Engine;
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(s.as_bytes())
                                .map_err(|e| {
                                    HelixError::new(format!("`base64_decode` got invalid base64: {e}"), line, col)
                                })?;
                            match String::from_utf8(bytes) {
                                Ok(text) => Ok(Value::Str(Rc::new(text))),
                                Err(_) => Err(HelixError::new(
                                    "`base64_decode` produced non-UTF-8 bytes — Helix strings are text, so binary payloads aren't representable",
                                    line,
                                    col,
                                )),
                            }
                        }
                        other => Err(type_err("base64_decode", "a string", other, line, col)),
                    }
                }
                "hex_encode" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) | Value::Dna(s) => {
                            Ok(Value::Str(Rc::new(s.bytes().map(|b| format!("{b:02x}")).collect())))
                        }
                        other => Err(type_err("hex_encode", "a string", other, line, col)),
                    }
                }
                "hex_decode" => {
                    arity(name, &args, 1, line, col)?;
                    match &args[0] {
                        Value::Missing => Ok(Value::Missing),
                        Value::Str(s) => match hex_to_bytes(s) {
                            Some(bytes) => match String::from_utf8(bytes) {
                                Ok(t) => Ok(Value::Str(Rc::new(t))),
                                Err(_) => Err(HelixError::new(
                                    "`hex_decode` produced non-UTF-8 bytes — Helix strings are text",
                                    line,
                                    col,
                                )),
                            },
                            None => Err(HelixError::new(
                                "`hex_decode` needs an even number of hex digits (0-9, a-f)",
                                line,
                                col,
                            )),
                        },
                        other => Err(type_err("hex_decode", "a hex string", other, line, col)),
                    }
                }
                // AES-256-GCM authenticated encryption. Misuse-resistant by construction: the
                // nonce is random per call (internal, prepended to the ciphertext) so it can't be
                // reused, and decryption verifies the tag — a wrong key or tampered ciphertext is
                // an error, never a silent garbage plaintext. Keys are 64-hex (use `aes_keygen()`).
                "aes_keygen" => {
                    arity(name, &args, 0, line, col)?;
                    use aes_gcm::{aead::OsRng, Aes256Gcm, KeyInit};
                    let key = Aes256Gcm::generate_key(&mut OsRng);
                    Ok(Value::Str(Rc::new(key.iter().map(|b| format!("{b:02x}")).collect())))
                }
                "aes_encrypt" => {
                    arity(name, &args, 2, line, col)?;
                    if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                        return Ok(Value::Missing);
                    }
                    let key_hex = match &args[0] {
                        Value::Str(s) => s,
                        other => return Err(type_err("aes_encrypt", "a 64-hex key", other, line, col)),
                    };
                    let plaintext = match &args[1] {
                        Value::Str(s) => s,
                        other => return Err(type_err("aes_encrypt", "a string to encrypt", other, line, col)),
                    };
                    let key_bytes = aes_key_bytes(key_hex, line, col)?;
                    use aes_gcm::{
                        aead::{Aead, AeadCore, OsRng},
                        Aes256Gcm, Key, KeyInit,
                    };
                    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
                    let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // 96-bit, unique per call
                    let ct = cipher
                        .encrypt(&nonce, plaintext.as_bytes())
                        .map_err(|_| HelixError::new("`aes_encrypt` failed", line, col))?;
                    let mut blob = nonce.to_vec();
                    blob.extend_from_slice(&ct);
                    use base64::Engine;
                    Ok(Value::Str(Rc::new(base64::engine::general_purpose::STANDARD.encode(&blob))))
                }
                "aes_decrypt" => {
                    arity(name, &args, 2, line, col)?;
                    if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                        return Ok(Value::Missing);
                    }
                    let key_hex = match &args[0] {
                        Value::Str(s) => s,
                        other => return Err(type_err("aes_decrypt", "a 64-hex key", other, line, col)),
                    };
                    let blob_b64 = match &args[1] {
                        Value::Str(s) => s,
                        other => return Err(type_err("aes_decrypt", "a base64 ciphertext", other, line, col)),
                    };
                    let key_bytes = aes_key_bytes(key_hex, line, col)?;
                    use base64::Engine;
                    let blob = base64::engine::general_purpose::STANDARD
                        .decode(blob_b64.as_bytes())
                        .map_err(|_| HelixError::new("`aes_decrypt` got invalid base64", line, col))?;
                    if blob.len() < 12 {
                        return Err(HelixError::new("`aes_decrypt` ciphertext is too short", line, col));
                    }
                    let (nonce, ct) = blob.split_at(12);
                    use aes_gcm::{aead::Aead, Aes256Gcm, Key, KeyInit, Nonce};
                    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
                    let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| {
                        HelixError::new(
                            "`aes_decrypt` failed — wrong key, or the ciphertext was tampered with",
                            line,
                            col,
                        )
                    })?;
                    match String::from_utf8(pt) {
                        Ok(t) => Ok(Value::Str(Rc::new(t))),
                        Err(_) => Err(HelixError::new("`aes_decrypt` produced non-UTF-8 plaintext", line, col)),
                    }
                }
                // Ed25519 signatures — the safe asymmetric primitive (deterministic, no nonce
                // footgun). keygen → {private, public} (both hex); sign → 128-hex signature;
                // verify → Bool (a wrong/forged signature is `false`, never an error). Strict
                // verification (rejects malleable signatures).
                "ed25519_keygen" => {
                    arity(name, &args, 0, line, col)?;
                    use aes_gcm::aead::{rand_core::RngCore, OsRng};
                    let mut seed = [0u8; 32];
                    OsRng.fill_bytes(&mut seed);
                    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
                    let pk = sk.verifying_key();
                    let priv_hex: String = sk.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    let pub_hex: String = pk.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    Ok(Value::Record(Rc::new(vec![
                        (crate::symbol::Symbol::intern("private"), Value::Str(Rc::new(priv_hex))),
                        (crate::symbol::Symbol::intern("public"), Value::Str(Rc::new(pub_hex))),
                    ])))
                }
                "ed25519_sign" => {
                    arity(name, &args, 2, line, col)?;
                    if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                        return Ok(Value::Missing);
                    }
                    let priv_hex = match &args[0] {
                        Value::Str(s) => s,
                        other => return Err(type_err("ed25519_sign", "a 64-hex private key", other, line, col)),
                    };
                    let msg = match &args[1] {
                        Value::Str(s) => s,
                        other => return Err(type_err("ed25519_sign", "a string message", other, line, col)),
                    };
                    let seed = hex_to_array32(priv_hex, "an ed25519 private key", line, col)?;
                    use ed25519_dalek::Signer;
                    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
                    let sig = sk.sign(msg.as_bytes());
                    Ok(Value::Str(Rc::new(sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect())))
                }
                "ed25519_verify" => {
                    arity(name, &args, 3, line, col)?;
                    if args.iter().any(|a| matches!(a, Value::Missing)) {
                        return Ok(Value::Missing);
                    }
                    let pub_hex = match &args[0] {
                        Value::Str(s) => s,
                        other => return Err(type_err("ed25519_verify", "a 64-hex public key", other, line, col)),
                    };
                    let msg = match &args[1] {
                        Value::Str(s) => s,
                        other => return Err(type_err("ed25519_verify", "a string message", other, line, col)),
                    };
                    let sig_hex = match &args[2] {
                        Value::Str(s) => s,
                        other => return Err(type_err("ed25519_verify", "a 128-hex signature", other, line, col)),
                    };
                    let pub_bytes = hex_to_array32(pub_hex, "an ed25519 public key", line, col)?;
                    let vk = match ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes) {
                        Ok(v) => v,
                        Err(_) => return Err(HelixError::new("`ed25519_verify` got an invalid public key", line, col)),
                    };
                    // A malformed signature is just an invalid one → `false`, not an error.
                    let sig_bytes: [u8; 64] = match hex_to_bytes(sig_hex).and_then(|b| b.try_into().ok()) {
                        Some(b) => b,
                        None => return Ok(Value::Bool(false)),
                    };
                    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                    // `verify_strict` rejects malleable (non-canonical) signatures.
                    Ok(Value::Bool(vk.verify_strict(msg.as_bytes(), &sig).is_ok()))
                }
        _ => Err(HelixError::new(
            format!("internal: `{name}` routed to the wrong builtin module"),
            line,
            col,
        )),
    }
}
