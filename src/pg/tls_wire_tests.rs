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

/// The same certificate signed `ecdsa-with-SHA256` — a signature that NAMES A HASH, so an
/// authentication can be bound to it (an Ed25519 signature names none; RFC 5929 has nothing to
/// say about it). The key is RFC 6979 §A.2.5's published P-256 test key — a test vector, not a
/// secret — and the signing is rustls's own, through the API a server signs its handshake with.
fn bindable_identity() -> (Vec<u8>, Vec<u8>) {
    let hex = |h: &str| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).expect("hex")).collect::<Vec<u8>>();
    let private = hex("C9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721");
    let mut point = vec![0u8, 4];
    point.extend(hex("60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6"));
    point.extend(hex("7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299"));
    let ec_p256 = seq(&[der(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]), der(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07])]);
    let pkcs8 = seq(&[
        der(0x02, &[0]),
        ec_p256.clone(),
        der(0x04, &seq(&[der(0x02, &[1]), der(0x04, &private), der(0xa1, &der(0x03, &point))])),
    ]);
    let ecdsa_sha256 = seq(&[der(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02])]);
    let name = seq(&[der(0x31, &seq(&[der(0x06, &[0x55, 0x04, 0x03]), der(0x0c, b"localhost")]))]);
    let san = seq(&[der(0x06, &[0x55, 0x1d, 0x11]), der(0x04, &seq(&[der(0x82, b"localhost")]))]);
    let tbs = seq(&[
        der(0xa0, &der(0x02, &[2])),
        der(0x02, &[2]),
        ecdsa_sha256.clone(),
        name.clone(),
        seq(&[der(0x17, b"200101000000Z"), der(0x18, b"20991231235959Z")]),
        name,
        seq(&[ec_p256, der(0x03, &point)]),
        der(0xa3, &seq(&[san])),
    ]);
    let key = rustls::crypto::ring::sign::any_ecdsa_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.clone())))
        .expect("the published P-256 test key, as PKCS#8");
    let signer = key.choose_scheme(&[rustls::SignatureScheme::ECDSA_NISTP256_SHA256]).expect("ECDSA P-256 / SHA-256");
    let mut signature = vec![0u8];
    signature.extend(signer.sign(&tbs).expect("signed"));
    (seq(&[tbs, ecdsa_sha256, der(0x03, &signature)]), pkcs8)
}

/// A peer that answers the SSLRequest the way a PostgreSQL server does and then speaks TLS;
/// `serve` gets the encrypted stream. Answers the address, and the certificate as the PEM
/// file a caller names in `sslrootcert=`.
fn tls_peer(
    serve: impl FnOnce(&mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>) + Send + 'static,
) -> (std::net::SocketAddr, String, std::thread::JoinHandle<()>) {
    tls_peer_as(localhost_identity(), serve)
}

/// The same, presenting this certificate.
fn tls_peer_as(
    (cert, pkcs8): (Vec<u8>, Vec<u8>),
    serve: impl FnOnce(&mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>) + Send + 'static,
) -> (std::net::SocketAddr, String, std::thread::JoinHandle<()>) {
    use base64::Engine as _;
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

// ── a PostgreSQL that can only log you in ────────────────────────────────────────────────────

type Peer = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

fn send(s: &mut Peer, tag: u8, body: &[u8]) {
    s.write_all(&framed(tag, body)).expect("write");
    s.flush().expect("flush");
}

fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// What the fake server saw the client ask for.
#[derive(Debug, Default, PartialEq)]
struct Asked {
    mechanism: String,
    /// The GS2 header the client-first message opened with.
    gs2: String,
}

/// THE SERVER SIDE OF A SCRAM LOGIN, written independently of `scram.rs` (HMAC and SHA-256 from
/// their crates, nothing of the client's): it offers `offered`, holds the client to the
/// channel it should have bound to — `its_certificate`, which is what a REAL server compares
/// with, and which a test can make differ from the certificate the client was shown — checks
/// the proof against `password`, and signs back. Anything wrong is an ErrorResponse, as it is
/// from PostgreSQL.
fn scram_server(s: &mut Peer, password: &str, offered: &[&str], its_certificate: &[u8], said: std::sync::mpsc::Sender<Asked>) {
    use base64::Engine as _;
    use sha2::Digest;
    let b64 = base64::engine::general_purpose::STANDARD;
    let read_tagged = |s: &mut Peer, tagged: bool| -> (u8, Vec<u8>) {
        let mut tag = [0u8; 1];
        if tagged {
            s.read_exact(&mut tag).expect("tag");
        }
        let mut len = [0u8; 4];
        s.read_exact(&mut len).expect("length");
        let mut body = vec![0u8; i32::from_be_bytes(len) as usize - 4];
        s.read_exact(&mut body).expect("body");
        (tag[0], body)
    };
    let fail = |s: &mut Peer, code: &str, text: &str| {
        let mut out = vec![b'S'];
        out.extend_from_slice(b"FATAL\0C");
        out.extend_from_slice(code.as_bytes());
        out.extend_from_slice(b"\0M");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\0\0");
        send(s, b'E', &out);
    };
    read_tagged(s, false); // the startup packet
    let mut list = 10i32.to_be_bytes().to_vec();
    for m in offered {
        list.extend_from_slice(m.as_bytes());
        list.push(0);
    }
    list.push(0);
    send(s, b'R', &list);

    // SASLInitialResponse: the mechanism, then the client-first message.
    let (_, body) = read_tagged(s, true);
    let nul = body.iter().position(|b| *b == 0).expect("mechanism");
    let mechanism = String::from_utf8_lossy(&body[..nul]).into_owned();
    let first = String::from_utf8_lossy(&body[nul + 5..]).into_owned();
    let bare_at = first.find("n=").expect("client-first-bare");
    let (gs2, bare) = (first[..bare_at].to_string(), first[bare_at..].to_string());
    let _ = said.send(Asked { mechanism: mechanism.clone(), gs2: gs2.clone() });
    let client_nonce = bare.split(",r=").nth(1).expect("nonce").to_string();

    let (salt, rounds) = (b"sixteen-byte-salt".to_vec(), 4096u32);
    let server_first = format!("r={client_nonce}SERVER,s={},i={rounds}", b64.encode(&salt));
    let mut cont = 11i32.to_be_bytes().to_vec();
    cont.extend_from_slice(server_first.as_bytes());
    send(s, b'R', &cont);

    let (_, body) = read_tagged(s, true);
    let last = String::from_utf8_lossy(&body).into_owned();
    let (without_proof, proof) = last.rsplit_once(",p=").expect("proof");
    let channel = without_proof.strip_prefix("c=").and_then(|r| r.split(',').next()).expect("c=");

    // What this server holds the client to: the header it opened with, and — bound — the hash
    // of the certificate THIS SERVER presented.
    let mut expected = gs2.as_bytes().to_vec();
    if mechanism == "SCRAM-SHA-256-PLUS" {
        expected.extend_from_slice(&sha2::Sha256::digest(its_certificate));
    }
    if gs2 == "y,," && offered.contains(&"SCRAM-SHA-256-PLUS") {
        return fail(s, "08P01", "the client supports SCRAM channel binding but thinks the server does not");
    }
    if b64.decode(channel).ok() != Some(expected) {
        return fail(s, "08P01", "SCRAM channel binding check failed");
    }
    // PBKDF2, one block; the keys; the proof.
    let mut block = salt.clone();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac256(password.as_bytes(), &block);
    let mut salted = u.clone();
    for _ in 1..rounds {
        u = hmac256(password.as_bytes(), &u);
        salted.iter_mut().zip(&u).for_each(|(o, x)| *o ^= x);
    }
    let client_key = hmac256(&salted, b"Client Key");
    let stored = sha2::Sha256::digest(&client_key).to_vec();
    let auth = format!("{bare},{server_first},{without_proof}");
    let signature = hmac256(&stored, auth.as_bytes());
    let given = b64.decode(proof).expect("proof is base64");
    let claimed: Vec<u8> = given.iter().zip(&signature).map(|(p, g)| p ^ g).collect();
    if sha2::Sha256::digest(&claimed).to_vec() != stored {
        return fail(s, "28P01", "password authentication failed");
    }
    let mut done = 12i32.to_be_bytes().to_vec();
    done.extend_from_slice(format!("v={}", b64.encode(hmac256(&hmac256(&salted, b"Server Key"), auth.as_bytes()))).as_bytes());
    send(s, b'R', &done);
    send(s, b'R', &0i32.to_be_bytes());
    send(s, b'S', b"server_version\x0017.0\x00");
    send(s, b'Z', b"I");
    // The client goes when its session does — with a goodbye or without one.
    let mut bye = [0u8; 1];
    let _ = s.read(&mut bye);
}

fn login(addr: std::net::SocketAddr, root: &str, password: &str, binding: super::conninfo::ChannelBinding) -> Result<(), String> {
    let target = super::conninfo::Target {
        host: "localhost".to_string(),
        port: addr.port(),
        user: "u".to_string(),
        password: password.to_string(),
        database: "db".to_string(),
        sslmode: super::conninfo::SslMode::VerifyFull,
        sslrootcert: Some(root.to_string()),
        patience: super::conninfo::Patience::Silence,
        channel_binding: binding,
        connect_timeout: Duration::from_secs(10),
    };
    super::connect::connect(&target, true).map(|_session| ())
}

/// A LOGIN OVER TLS IS BOUND TO THE CERTIFICATE. The server offers `-PLUS`, the client takes it
/// and signs the hash of the certificate it was shown, and a server that compares that with
/// the certificate it presented lets it in. With the WRONG password the same exchange is a
/// password error and nothing more — the hint about certificates is not for that.
#[test]
fn a_login_over_tls_is_bound_to_the_certificate() {
    use super::conninfo::ChannelBinding::{Prefer, Require};
    for (password, asked) in [("pencil", Prefer), ("pencil", Require), ("not the password", Prefer)] {
        let (cert, key) = bindable_identity();
        let presented = cert.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let (addr, root, h) = tls_peer_as((cert, key), move |s| {
            scram_server(s, "pencil", &["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"], &presented, tx)
        });
        let got = login(addr, &root, password, asked);
        assert_eq!(
            rx.recv().expect("the server saw a login"),
            Asked { mechanism: "SCRAM-SHA-256-PLUS".to_string(), gs2: "p=tls-server-end-point,,".to_string() }
        );
        match password {
            "pencil" => got.unwrap_or_else(|e| panic!("a bound login: {e}")),
            _ => {
                let e = got.expect_err("the wrong password");
                assert!(e.contains("28P01") && !e.contains("channel_binding"), "{e}");
            }
        }
        h.join().expect("the server finished");
        let _ = std::fs::remove_file(root);
    }
}

/// A CERTIFICATE IN THE MIDDLE. The client is shown one certificate and the server compares the
/// binding with another — what a relay holding a certificate of its own looks like from both
/// ends. TLS verified and the password was right, and the login FAILS: that is channel binding
/// doing the one thing it is for. The error says what it means, and what to write if the thing
/// in the middle is a proxy somebody put there on purpose.
#[test]
fn a_certificate_in_the_middle_fails_the_login_and_says_what_to_do() {
    let (cert, key) = bindable_identity();
    let (tx, rx) = std::sync::mpsc::channel();
    let (addr, root, h) = tls_peer_as((cert, key), move |s| {
        scram_server(s, "pencil", &["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"], b"the certificate the real server holds", tx)
    });
    let e = login(addr, &root, "pencil", super::conninfo::ChannelBinding::Prefer).expect_err("not the same channel");
    assert!(e.contains("channel binding check failed"), "{e}");
    assert!(e.contains("`channel_binding=disable`"), "{e}");
    assert_eq!(rx.recv().expect("seen").mechanism, "SCRAM-SHA-256-PLUS");
    h.join().expect("the server finished");

    // And said so, the same relay lets the login through: unbound, as asked.
    let (cert, key) = bindable_identity();
    let (tx, rx) = std::sync::mpsc::channel();
    let (addr, root2, h) = tls_peer_as((cert, key), move |s| {
        scram_server(s, "pencil", &["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"], b"the certificate the real server holds", tx)
    });
    login(addr, &root2, "pencil", super::conninfo::ChannelBinding::Disable).expect("unbound, as the URL asked");
    assert_eq!(rx.recv().expect("seen"), Asked { mechanism: "SCRAM-SHA-256".to_string(), gs2: "n,,".to_string() });
    h.join().expect("the server finished");
    let _ = std::fs::remove_file(root);
    let _ = std::fs::remove_file(root2);
}

/// WHAT THE CLIENT SAYS WHEN IT DOES NOT BIND. Not offered `-PLUS` although it could have bound,
/// it says so (`y,,`) — which is what lets a server that DID offer, before something removed
/// the offer on the way, refuse. Shown a certificate whose signature names no hash (Ed25519),
/// it cannot bind at all and says that (`n,,`), and logs in; asked to REQUIRE binding, it
/// refuses before a password-derived byte is sent.
#[test]
fn a_login_that_is_not_bound_says_why() {
    use super::conninfo::ChannelBinding::{Prefer, Require};
    // Could have bound; was not offered the chance.
    let (cert, key) = bindable_identity();
    let presented = cert.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let (addr, root, h) = tls_peer_as((cert, key), move |s| scram_server(s, "pencil", &["SCRAM-SHA-256"], &presented, tx));
    login(addr, &root, "pencil", Prefer).expect("an old server is still a server");
    assert_eq!(rx.recv().expect("seen"), Asked { mechanism: "SCRAM-SHA-256".to_string(), gs2: "y,,".to_string() });
    h.join().expect("the server finished");
    let _ = std::fs::remove_file(root);

    // A certificate nothing can be bound to.
    let (cert, key) = localhost_identity();
    let presented = cert.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let (addr, root, h) =
        tls_peer_as((cert, key), move |s| scram_server(s, "pencil", &["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"], &presented, tx));
    login(addr, &root, "pencil", Prefer).expect("unbound, and in");
    assert_eq!(rx.recv().expect("seen"), Asked { mechanism: "SCRAM-SHA-256".to_string(), gs2: "n,,".to_string() });
    h.join().expect("the server finished");
    let _ = std::fs::remove_file(root);

    // And `require` will not settle for it.
    let (cert, key) = localhost_identity();
    let (addr, root, h) = tls_peer_as((cert, key), move |s| {
        let mut startup = [0u8; 4];
        let _ = s.read_exact(&mut startup);
        let mut list = 10i32.to_be_bytes().to_vec();
        list.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
        // The rest of the startup packet is still unread; the client hangs up before it matters.
        let _ = s.write_all(&framed(b'R', &list));
        let _ = s.flush();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        assert!(!rest.windows(2).any(|w| w == b"p="), "nothing of the password left the client");
    });
    let e = login(addr, &root, "pencil", Require).expect_err("not bound, so not at all");
    assert!(e.contains("names no hash to bind to"), "{e}");
    h.join().expect("the server finished");
    let _ = std::fs::remove_file(root);
}
