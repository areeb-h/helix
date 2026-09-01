//! TLS for the PostgreSQL connection.
//!
//! WHAT THE OTHER CLIENTS DO, AND WHY THIS DOES NOT. `libpq` defaults to `sslmode=prefer`,
//! and so does Go's `pgx`: the client asks for TLS, and **if the server says no, the
//! session continues in plaintext**. An attacker on the path does not need to break TLS;
//! they answer `N` to the SSLRequest and read the password exchange. `require` is the next
//! rung and is barely better — it encrypts, but verifies no certificate, so anyone who can
//! answer on port 5432 can present any certificate at all and be believed. Six modes exist
//! (`disable`, `allow`, `prefer`, `require`, `verify-ca`, `verify-full`) and four of them
//! are traps with names.
//!
//! Helix takes two:
//!
//!   * **`verify-full`** — the default, and what you get by writing nothing. TLS is
//!     mandatory, the chain must reach a trusted root, and the certificate must match the
//!     host you asked for.
//!   * **`disable`** — plaintext, spelled out in the URL by the person who wants it.
//!
//! The property that matters is that **the server can never cause the downgrade**. There
//! is no mode in which a `N` byte is an acceptable answer, so the choice between encrypted
//! and not is made once, in the caller's own URL, and cannot be revised by anything on the
//! network. Everything a rejected mode would have given you is reachable: a private CA is
//! a *file* (`sslrootcert=`), not a switch that turns checking off.
//!
//! ANCHORS. The Mozilla root set (`webpki-roots`), which is the same set the HTTP client
//! already trusts — one trust story for the whole binary, and no dependency on the host
//! having a populated certificate store. `sslrootcert=<path>` REPLACES that set rather
//! than adding to it, matching `libpq`, so pinning a provider's CA means exactly that.
//!
//! NO NEW CRATES. `rustls`, its `ring` provider, and `webpki-roots` are already compiled
//! into the binary for HTTPS; this makes them direct dependencies of the `postgres`
//! feature so that `--no-default-features --features postgres` is self-sufficient.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// The SSLRequest packet's body: protocol 1234.5679, the reserved version number that
/// means "before anything else, may we start TLS?". Frozen since 7.2.
const SSL_REQUEST: i32 = 80877103;

/// The connection, before or after TLS wraps it.
///
/// `proto`'s framing already takes `impl Read`/`impl Write`, so this is the only place
/// that has to know which one it is — every message-level function reads and writes the
/// same way whether or not there is a TLS record layer underneath.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

impl Stream {
    /// Whether this connection is encrypted — for the diagnostic label, so a program that
    /// prints a connection says which kind it has.
    pub fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }

    /// Say goodbye at the TLS layer as well as the protocol one.
    ///
    /// Failure is ignored: this runs from `Drop`, which must not raise, and a session
    /// that cannot be closed politely is still closed when the descriptor goes.
    pub fn close_notify(&mut self) {
        if let Stream::Tls(s) = self {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }
}

/// Ask the server for TLS and wrap the socket, or fail.
///
/// The one-byte reply is the whole negotiation: `S` proceed, `N` refuse. `N` is an ERROR
/// here and never a fallback — that is the downgrade this client does not have.
pub fn negotiate(mut tcp: TcpStream, host: &str, root: Option<&str>) -> Result<Stream, String> {
    let mut pkt = [0u8; 8];
    pkt[..4].copy_from_slice(&8i32.to_be_bytes());
    pkt[4..].copy_from_slice(&SSL_REQUEST.to_be_bytes());
    tcp.write_all(&pkt).map_err(|e| format!("sending the TLS request: {e}"))?;
    tcp.flush().map_err(|e| format!("sending the TLS request: {e}"))?;

    let mut reply = [0u8; 1];
    tcp.read_exact(&mut reply).map_err(|e| format!("waiting for the TLS reply: {e}"))?;
    match reply[0] {
        b'S' => {}
        b'N' => {
            return Err(
                "the server refused TLS. Helix does not continue in plaintext when TLS was \
                 asked for — that is the downgrade `sslmode=prefer` allows, and it is how a \
                 password exchange ends up readable on the wire. Turn TLS on at the server \
                 (`ssl = on`), or say `?sslmode=disable` in the URL if this connection is \
                 genuinely not worth protecting"
                    .to_string(),
            )
        }
        // A pre-7.2 server, or something that is not PostgreSQL. `E` is an ErrorResponse,
        // which some poolers answer with.
        other => {
            return Err(format!(
                "the server answered the TLS request with `{}` (0x{other:02x}), which is \
                 neither `S` nor `N` — it may not be a PostgreSQL server",
                other as char
            ))
        }
    }

    let store = anchors(root)?;
    // `ClientConfig::builder()` PANICS when no process-wide crypto provider has been
    // installed, and ADR 0024 says user input must never abort the host. Naming the
    // provider here removes that dependence on global state entirely.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS configuration: {e}"))?
        .with_root_certificates(store)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| format!("`{host}` is not a name a certificate can be checked against"))?;
    let conn = ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("starting TLS: {e}"))?;

    let mut s = StreamOwned::new(conn, tcp);
    // Drive the handshake NOW rather than letting the first startup byte trigger it, so a
    // certificate that does not verify is reported as a connection failure with the
    // reason, instead of surfacing halfway through authentication.
    s.flush().map_err(|e| format!("TLS handshake with `{host}`: {e}"))?;
    Ok(Stream::Tls(Box::new(s)))
}

/// The trust anchors: the Mozilla set, or exactly the certificates in `root`.
fn anchors(root: Option<&str>) -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    let Some(path) = root else {
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        return Ok(store);
    };
    let pem = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the root certificate `{path}`: {e}"))?;
    let certs = pem_certs(&pem, path)?;
    if certs.is_empty() {
        return Err(format!(
            "`{path}` contains no certificate — a root certificate file holds one or more \
             `-----BEGIN CERTIFICATE-----` blocks"
        ));
    }
    for c in certs {
        store.add(c).map_err(|e| format!("`{path}` is not a usable certificate: {e}"))?;
    }
    Ok(store)
}

/// Every `CERTIFICATE` block in a PEM file, as DER.
///
/// Hand-rolled rather than adding `rustls-pemfile`, and the reason it is safe to is that
/// this parser cannot fail OPEN: its only outputs are "these bytes are a certificate" and
/// an error. A malformed block yields no anchor, so the connection then fails to verify —
/// the direction a trust-store bug has to fail in.
fn pem_certs(pem: &str, path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    use base64::Engine as _;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let engine = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(at) = rest.find(BEGIN) {
        let after = &rest[at + BEGIN.len()..];
        let end = after
            .find(END)
            .ok_or_else(|| format!("`{path}` has a BEGIN CERTIFICATE with no matching END"))?;
        let body: String = after[..end].chars().filter(|c| !c.is_whitespace()).collect();
        let der = engine
            .decode(body.as_bytes())
            .map_err(|e| format!("`{path}` has a certificate that is not valid base64: {e}"))?;
        out.push(CertificateDer::from(der));
        rest = &after[end + END.len()..];
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// `expect_err` needs `Debug` on the Ok side, and rustls' types do not have it.
    fn refused<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("this must not have succeeded"),
            Err(e) => e,
        }
    }

    /// A server that reads the SSLRequest and answers with one byte of our choosing.
    fn one_byte_server(reply: u8) -> String {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut req = [0u8; 8];
                let _ = c.read_exact(&mut req);
                // The client must have asked the right question first.
                assert_eq!(i32::from_be_bytes([req[0], req[1], req[2], req[3]]), 8);
                assert_eq!(
                    i32::from_be_bytes([req[4], req[5], req[6], req[7]]),
                    SSL_REQUEST
                );
                let _ = c.write_all(&[reply]);
                let _ = c.flush();
            }
        });
        addr
    }

    /// A SERVER CANNOT CAUSE A DOWNGRADE. This is the whole difference from `libpq`'s
    /// default (`sslmode=prefer`, which Go's `pgx` shares): there, an `N` here means the
    /// session continues in plaintext and the password exchange that follows is readable
    /// by anyone on the path. Here it is the end of the connection.
    #[test]
    fn a_server_refusing_tls_is_an_error_and_never_a_fallback() {
        let addr = one_byte_server(b'N');
        let tcp = TcpStream::connect(&addr).expect("connect");
        let e = refused(negotiate(tcp, "localhost", None));
        assert!(e.contains("refused TLS"), "{e}");
        // The message has to name the fix, or it is just a refusal.
        assert!(e.contains("sslmode=disable"), "{e}");
    }

    /// Anything that is neither `S` nor `N` is also not a reason to continue.
    #[test]
    fn an_unrecognised_tls_reply_is_an_error() {
        let addr = one_byte_server(b'E');
        let tcp = TcpStream::connect(&addr).expect("connect");
        let e = refused(negotiate(tcp, "localhost", None));
        assert!(e.contains("neither `S` nor `N`"), "{e}");
    }

    /// The PEM reader can fail CLOSED but never OPEN: its only outputs are certificates
    /// and errors, so a malformed anchor file yields no trust rather than misplaced trust.
    #[test]
    fn a_root_certificate_file_cannot_fail_open() {
        let dir = std::env::temp_dir().join(format!("hx_pem_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let write = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, body).expect("write");
            p.to_str().expect("utf8").to_string()
        };

        // A file with no certificate in it is refused, not treated as "trust nothing
        // extra" — which would silently fall back to the default anchors.
        let empty = write("empty.pem", "# nothing here\n");
        let e = refused(anchors(Some(&empty)));
        assert!(e.contains("no certificate"), "{e}");

        // A truncated block is refused rather than partially read.
        let cut = write("cut.pem", "-----BEGIN CERTIFICATE-----\nMIIB\n");
        let e = refused(anchors(Some(&cut)));
        assert!(e.contains("no matching END"), "{e}");

        // Base64 that is not a certificate is refused by rustls, not accepted as one.
        let junk = write(
            "junk.pem",
            "-----BEGIN CERTIFICATE-----\naGVsbG8gd29ybGQ=\n-----END CERTIFICATE-----\n",
        );
        assert!(anchors(Some(&junk)).is_err(), "junk DER must not become an anchor");

        // A file that is not there at all names the path.
        let e = refused(anchors(Some("/nonexistent/ca.pem")));
        assert!(e.contains("/nonexistent/ca.pem"), "{e}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
