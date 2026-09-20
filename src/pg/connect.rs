//! Opening a session: the socket, TLS, the startup packet, authentication, and the wait for
//! the server to say it is ready.

use std::net::TcpStream;
use std::time::Duration;

use super::conninfo::{Patience, SslMode, Target};
use super::proto::{error_text, put_cstr, write_msg, Cur};
use super::statement::Session;
use super::stream::Stream;
use super::{scram, tls};

/// Protocol 3.0, as `libpq` still requests by default.
const PROTOCOL_3_0: i32 = 196_608;

/// How long the HANDSHAKE waits for each reply — and how long the server may be silent
/// afterwards, when the URL says nothing about it. A server that accepts a connection and then
/// says nothing is broken, not busy, so the handshake's bound is not the URL's to move.
const SILENCE: Duration = Duration::from_secs(30);

/// How long past a statement's own limit the client keeps listening before it concludes the
/// server is not going to answer at all.
const GRACE: Duration = Duration::from_secs(10);

/// What the error says when the default silence runs out: the wait is the URL's to change.
const WAY_OUT: &str = " — a statement that needs longer says so in its connection's URL: \
                       `?timeout=300` lets one run five minutes (the server then ends an over-long \
                       statement itself, and the connection carries on), `timeout=0` as long as it takes";

/// Connect, authenticate, and leave the session ready for a query.
pub fn connect(t: &Target, read_only: bool) -> Result<Session, String> {
    let addr = format!("{}:{}", t.host, t.port);
    let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| format!("cannot resolve `{addr}`: {e}"))?
        .collect();
    let first = addrs.first().ok_or_else(|| format!("`{addr}` resolved to no address"))?;
    let s = TcpStream::connect_timeout(first, t.connect_timeout)
        .map_err(|e| format!("cannot connect to `{addr}`: {e}"))?;
    // A bounded wait, so a server that accepts and then stalls cannot hang the program.
    s.set_read_timeout(Some(SILENCE)).map_err(|e| format!("setting a read timeout: {e}"))?;
    s.set_write_timeout(Some(SILENCE)).map_err(|e| format!("setting a write timeout: {e}"))?;
    // Small messages, and latency is what matters on a query round trip.
    let _ = s.set_nodelay(true);
    keep_alive(&s);

    // TLS FIRST, before the startup packet — which is the message carrying the user name,
    // and which is immediately followed by the password exchange. The negotiation is one
    // byte and it is not a preference: a server that answers "no" ends the connection
    // here rather than continuing in the clear.
    let mut s = match t.sslmode {
        SslMode::Disable => Stream::plain(s, Some(SILENCE)),
        SslMode::VerifyFull => tls::negotiate(s, &t.host, t.sslrootcert.as_deref(), Some(SILENCE))?,
    };

    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_3_0.to_be_bytes());
    put_cstr(&mut body, "user");
    put_cstr(&mut body, &t.user);
    put_cstr(&mut body, "database");
    put_cstr(&mut body, &t.database);
    put_cstr(&mut body, "application_name");
    put_cstr(&mut body, "helix");
    put_cstr(&mut body, "client_encoding");
    put_cstr(&mut body, "UTF8");
    // READ-ONLY FROM THE FIRST BYTE — unless this session was opened to write. Sending
    // this as a startup parameter rather than as a `begin transaction read only` means the
    // session is read-only before a single statement can be sent — there is no window,
    // not even a short one — and it costs ZERO round trips where the explicit transaction
    // cost two (begin and commit). A writable session (`postgres_execute`,
    // `postgres_open(url, "write")`) simply omits it — the server's own default is
    // read-write — and the `db-write` grant has been checked before this packet is built
    // (ADR 0047).
    if read_only {
        put_cstr(&mut body, "default_transaction_read_only");
        put_cstr(&mut body, "on");
    }
    // A LIMIT ON HOW LONG A STATEMENT RUNS IS THE SERVER'S TO ENFORCE. This client's only
    // bound used to be its own read timeout — thirty seconds of silence, not the caller's to
    // move — and a statement that outlived it was abandoned: the connection closed, because
    // its reply was still coming, and the server left working on an answer nobody would read.
    // `timeout=N` asks the SERVER instead (`statement_timeout`, from the first byte and for no
    // round trip, like read-only above): running too long is then an ORDINARY ERROR — the
    // server stops the statement itself, says so (`57014`), and the connection carries on.
    // The client's own wait sits a little past the limit, so the server's verdict arrives
    // first and the wait is only ever what it should be: a bound on a server that has stopped
    // answering altogether.
    if let Patience::Limit(limit) = t.patience {
        put_cstr(&mut body, "statement_timeout");
        put_cstr(&mut body, &limit.as_millis().to_string());
    }
    body.push(0);
    write_msg(&mut s, None, &body)?;

    authenticate(&mut s, t)?;
    let exact_float_text = until_ready(&mut s)?;
    let mut session = Session::new(s, exact_float_text);
    match t.patience {
        // Nothing asked: nothing changes, except that the error now names the way out.
        Patience::Silence => session.stream.set_wait(Some(SILENCE), WAY_OUT)?,
        Patience::Limit(limit) => {
            session.stream.set_wait(Some(limit + GRACE), "")?;
            session.limit = Some(limit);
        }
        // As long as it takes — which `keep_alive` keeps from meaning "forever" when there is
        // nobody left to answer.
        Patience::Unbounded => session.stream.set_wait(None, "")?,
    }
    Ok(session)
}

/// Ask the kernel to notice a peer that has gone — a host that lost power, a NAT that forgot
/// the flow — by probing a connection that has been quiet for a minute. With `timeout=0` a
/// statement may wait as long as it takes, and this is what keeps "as long as it takes" from
/// meaning "forever" when there is nobody left to answer; on a long-lived connection it turns
/// the next statement's long wait into a prompt error. Best effort: a platform or a socket
/// that refuses it is no worse off than before.
#[cfg(unix)]
fn keep_alive(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = s.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| {
        // SAFETY: `fd` is this live socket's descriptor for the whole call, and the option
        // value is a `c_int` passed by pointer with its own size — the documented contract
        // of `setsockopt` for every option set here.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    // Quiet for a minute, then a probe every ten seconds, and six unanswered is a dead peer.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 60);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, 60);
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 6);
    }
}

#[cfg(not(unix))]
fn keep_alive(_: &TcpStream) {}

/// Read to the first `ReadyForQuery`, keeping the one thing the server says about itself that
/// this client acts on: whether its `float8` text is exact. `NoticeResponse`,
/// `ParameterStatus` and `BackendKeyData` can arrive at ANY time by the protocol's own rules,
/// so the wait tolerates them rather than treating them as the reply.
fn until_ready(s: &mut Stream) -> Result<bool, String> {
    let mut exact_float_text = false;
    loop {
        let (tag, body) = s.next_msg()?;
        match tag {
            b'E' => return Err(error_text(body)),
            // ParameterStatus: a name and its value.
            b'S' => {
                let mut c = Cur::new(body);
                if c.cstr_ref()? == "server_version" {
                    exact_float_text = prints_floats_exactly(c.cstr_ref()?);
                }
            }
            b'Z' => return Ok(exact_float_text),
            _ => continue,
        }
    }
}

/// PostgreSQL 12 made a float's text the shortest decimal that reads back as the same float;
/// before it the default was 15 significant digits, which does not. `server_version` is
/// `17.2`, `12.1 (Debian 12.1-1)`, `19beta3`, `9.6.24` — the major version is its leading
/// digits, and a version that cannot be read is taken for an old one.
fn prints_floats_exactly(server_version: &str) -> bool {
    let digits: String = server_version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().is_ok_and(|major| major >= 12)
}

fn authenticate(s: &mut Stream, t: &Target) -> Result<(), String> {
    let mut sasl: Option<scram::Scram> = None;
    loop {
        let (tag, body) = s.next_msg()?;
        match tag {
            b'E' => return Err(error_text(body)),
            b'R' => {
                let mut c = Cur::new(body);
                match c.i32()? {
                    // AuthenticationOk
                    0 => return Ok(()),
                    // SASL: a list of mechanisms. Only SCRAM-SHA-256 is offered back.
                    10 => {
                        let mut names = Vec::new();
                        loop {
                            let n = c.cstr()?;
                            if n.is_empty() {
                                break;
                            }
                            names.push(n);
                        }
                        if !names.iter().any(|n| n == "SCRAM-SHA-256") {
                            return Err(format!(
                                "the server offers only {} for authentication; this client speaks SCRAM-SHA-256",
                                names.join(", ")
                            ));
                        }
                        let mut sc = scram::Scram::new(&t.password);
                        let first = sc.client_first();
                        let mut out = Vec::new();
                        put_cstr(&mut out, "SCRAM-SHA-256");
                        out.extend_from_slice(&(first.len() as i32).to_be_bytes());
                        out.extend_from_slice(first.as_bytes());
                        write_msg(s, Some(b'p'), &out)?;
                        sasl = Some(sc);
                    }
                    // SASLContinue
                    11 => {
                        let server_first = std::str::from_utf8(c.rest())
                            .map_err(|_| "the server's SCRAM challenge is not UTF-8".to_string())?
                            .to_string();
                        let sc = sasl.as_mut().ok_or("the server continued a SASL exchange that never started")?;
                        let final_msg = sc.client_final(&server_first)?;
                        write_msg(s, Some(b'p'), final_msg.as_bytes())?;
                    }
                    // SASLFinal — verified, not assumed.
                    12 => {
                        let sc = sasl.as_ref().ok_or("the server finished a SASL exchange that never started")?;
                        let server_final = std::str::from_utf8(c.rest())
                            .map_err(|_| "the server's SCRAM signature is not UTF-8".to_string())?;
                        sc.verify_server(server_final)?;
                    }
                    // Cleartext and MD5 are refused BY NAME rather than supported. MD5 is
                    // deprecated upstream, and a client that silently downgrades when asked
                    // is the whole problem with offering it.
                    3 => return Err("the server asked for a cleartext password; this client requires SCRAM-SHA-256".into()),
                    5 => return Err("the server asked for MD5 authentication, which is deprecated; set `password_encryption = scram-sha-256`".into()),
                    other => return Err(format!("the server asked for authentication method {other}, which this client does not implement")),
                }
            }
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::prints_floats_exactly;

    /// The kernel is asked to probe a quiet connection — read back from the socket itself.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_connection_asks_the_kernel_to_notice_a_dead_peer() {
        use std::os::fd::AsRawFd;
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let s = std::net::TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
        super::keep_alive(&s);
        let get = |level: libc::c_int, name: libc::c_int| {
            let mut value: libc::c_int = -1;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: a live descriptor, and a `c_int` with its size — `getsockopt`'s contract.
            let rc = unsafe {
                libc::getsockopt(s.as_raw_fd(), level, name, &mut value as *mut libc::c_int as *mut libc::c_void, &mut len)
            };
            assert_eq!(rc, 0);
            value
        };
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), 60);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), 10);
        assert_eq!(get(libc::IPPROTO_TCP, libc::TCP_KEEPCNT), 6);
    }

    #[test]
    fn a_float_is_binary_only_from_a_server_whose_text_is_exact() {
        for v in ["12.0", "12devel", "17.2", "17.11 (Debian 17.11-1.pgdg120+1)", "19beta3", "100.1"] {
            assert!(prints_floats_exactly(v), "{v}");
        }
        for v in ["11.22", "9.6.24", "8.4", "", "unknown", "v17"] {
            assert!(!prints_floats_exactly(v), "{v}");
        }
    }
}
