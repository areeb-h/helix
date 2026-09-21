//! TLS FOR REAL, INSIDE THE GATE. The driver's TLS data path had no test that runs without a
//! server: the policy was pinned (`conninfo`, `tls`), and the bytes under it were verified by
//! hand against a live PostgreSQL. Now that a flight is sent on a socket that does not block —
//! which under TLS means driving rustls's record layer directly — that is not enough.
//!
//! No server is needed, only a peer that speaks TLS: rustls's own server side, already compiled
//! in, behind the one-byte `S` a PostgreSQL server answers the SSLRequest with. Its certificate
//! is made HERE, at test time — a self-signed Ed25519 certificate for `localhost`, the DER
//! written out by hand and signed with `ed25519-dalek` (both already in the tree) — so no key
//! is ever checked in, and the client trusts it the only way this client trusts anything: as a
//! `sslrootcert` file, through `tls::negotiate`, chain and name verified.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::Signer;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::stream::Stream;

/// One DER element: tag, length (short or long form), body.
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

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

/// A self-signed certificate for `localhost` (X.509 v3, Ed25519, valid 2020–2099, one
/// subjectAltName) and its key as PKCS#8 — from a fixed seed, so a failure reproduces.
fn localhost_identity() -> (Vec<u8>, Vec<u8>) {
    let seed = [0x5au8; 32];
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let ed25519 = seq(&[der(0x06, &[0x2b, 0x65, 0x70])]); // 1.3.101.112
    let name = seq(&[der(0x31, &seq(&[der(0x06, &[0x55, 0x04, 0x03]), der(0x0c, b"localhost")]))]);
    let validity = seq(&[der(0x17, b"200101000000Z"), der(0x18, b"20991231235959Z")]);
    let mut public = vec![0u8];
    public.extend_from_slice(key.verifying_key().as_bytes());
    let spki = seq(&[ed25519.clone(), der(0x03, &public)]);
    let san = seq(&[der(0x06, &[0x55, 0x1d, 0x11]), der(0x04, &seq(&[der(0x82, b"localhost")]))]);
    let tbs = seq(&[
        der(0xa0, &der(0x02, &[2])), // v3
        der(0x02, &[1]),             // serial
        ed25519.clone(),
        name.clone(), // issuer: itself
        validity,
        name,
        spki,
        der(0xa3, &seq(&[san])),
    ]);
    let mut signature = vec![0u8];
    signature.extend_from_slice(&key.sign(&tbs).to_bytes());
    let cert = seq(&[tbs, ed25519.clone(), der(0x03, &signature)]);
    let pkcs8 = seq(&[der(0x02, &[0]), ed25519, der(0x04, &der(0x04, &seed))]);
    (cert, pkcs8)
}

/// A peer that answers the SSLRequest the way a PostgreSQL server does and then speaks TLS;
/// `serve` gets the encrypted stream. Answers the address, and the certificate as the PEM
/// file a caller names in `sslrootcert=`.
fn tls_peer(
    serve: impl FnOnce(&mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>) + Send + 'static,
) -> (std::net::SocketAddr, String, std::thread::JoinHandle<()>) {
    use base64::Engine as _;
    let (cert, pkcs8) = localhost_identity();
    let pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        base64::engine::general_purpose::STANDARD.encode(&cert)
    );
    let path = std::env::temp_dir().join(format!("hx_tls_peer_{}_{:?}.pem", std::process::id(), std::thread::current().id()));
    std::fs::write(&path, pem).expect("write the root certificate");

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)))
        .expect("the certificate made above is one rustls will serve");
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    let h = std::thread::spawn(move || {
        let (mut tcp, _) = l.accept().expect("accept");
        tcp.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(20))).expect("timeout");
        let mut request = [0u8; 8];
        tcp.read_exact(&mut request).expect("the SSLRequest");
        tcp.write_all(b"S").expect("yes to TLS");
        let conn = rustls::ServerConnection::new(Arc::new(config)).expect("a server connection");
        serve(&mut rustls::StreamOwned::new(conn, tcp));
    });
    (addr, path.to_str().expect("utf-8 temp path").to_string(), h)
}

fn connect(addr: std::net::SocketAddr, root: &str) -> Stream {
    let tcp = TcpStream::connect(addr).expect("connect");
    let wait = Duration::from_secs(20);
    tcp.set_read_timeout(Some(wait)).expect("timeout");
    tcp.set_write_timeout(Some(wait)).expect("timeout");
    // The name verified is `localhost`, whatever address was dialled — as with `sslrootcert=`.
    super::tls::negotiate(tcp, "localhost", Some(root), Some(wait)).unwrap_or_else(|e| panic!("TLS to the test peer: {e}"))
}

fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Messages of every awkward size come back whole and in order through a real TLS session —
/// empty, tiny, a buffer's worth to the byte, and larger than the buffer and than a TLS record.
#[test]
fn messages_arrive_whole_through_tls() {
    let sizes = [0usize, 1, 5, 300, 16 * 1024 - 5, 16 * 1024, 16 * 1024 + 1, 70_000, 2, 0, 40_000];
    let body_of = |i: usize, n: usize| (0..n).map(|k| ((k * 13 + i) % 251) as u8).collect::<Vec<u8>>();
    let (addr, root, h) = tls_peer(move |s| {
        for (i, n) in sizes.iter().enumerate() {
            s.write_all(&framed(b'a' + i as u8, &body_of(i, *n))).expect("write");
        }
        s.flush().expect("flush");
        s.conn.send_close_notify();
        let _ = s.flush();
    });
    let mut s = connect(addr, &root);
    assert!(s.is_tls());
    for (i, n) in sizes.iter().enumerate() {
        let (tag, body) = s.next_msg().unwrap_or_else(|e| panic!("message {i}: {e}"));
        assert_eq!(tag, b'a' + i as u8);
        assert!(body == body_of(i, *n).as_slice(), "message {i} of {n} bytes");
    }
    assert!(s.next_msg().is_err(), "and then the peer has gone");
    h.join().expect("the peer finished");
    let _ = std::fs::remove_file(root);
}

/// THE DEADLOCK, UNDER TLS. The peer sends megabytes before it reads a byte of the request and
/// will not read until it has; `send_draining` drives the record layer on a socket that does
/// not block — plaintext in, records out, records in, plaintext set aside — and then every
/// message is there, in order. (With a write-first client both ends wait for ever; the
/// plaintext twin of this test, in `stream`, was shown to fail that way.)
#[test]
fn a_request_answered_before_it_is_all_sent_does_not_deadlock_under_tls() {
    const MESSAGES: usize = 400;
    let body_of = |i: usize| (0..10_000).map(|k| ((k * 7 + i) % 251) as u8).collect::<Vec<u8>>();
    let request: Vec<u8> = (0..4_000_000usize).map(|k| (k % 253) as u8).collect();
    let expected = request.clone();
    let (addr, root, h) = tls_peer(move |s| {
        for i in 0..MESSAGES {
            s.write_all(&framed(b'D', &body_of(i))).expect("the client is taking the answer in");
        }
        s.flush().expect("flush");
        let mut got = vec![0u8; expected.len()];
        s.read_exact(&mut got).expect("the whole request arrives");
        assert!(got == expected, "the request arrived intact");
        s.write_all(&framed(b'Z', b"I")).expect("ready");
        s.flush().expect("flush");
    });
    let mut s = connect(addr, &root);
    s.send_draining(&request).expect("sent without waiting on a peer that is waiting on us");
    for i in 0..MESSAGES {
        let (tag, body) = s.next_msg().unwrap_or_else(|e| panic!("message {i}: {e}"));
        assert_eq!(tag, b'D');
        assert!(body == body_of(i).as_slice(), "message {i} is what was sent, in order");
    }
    assert_eq!(s.next_msg().expect("ready").0, b'Z');
    h.join().expect("the peer finished");
    let _ = std::fs::remove_file(root);
}
