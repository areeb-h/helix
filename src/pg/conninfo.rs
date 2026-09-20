//! The connection URL, and the security policy it carries.
//!
//! Separate from the socket work on purpose. `sslmode`'s accepted values ARE the policy —
//! which spellings mean encrypted, which mean plaintext, and which are refused because
//! they let something other than the caller decide. A policy is only as good as the tests
//! that run, and the gate does not build the `postgres` feature, so a policy living inside
//! that feature would be one nothing ever checked. Nothing here needs a socket, so nothing
//! here is gated: this compiles, and is tested, in every build.

use std::time::Duration;

use crate::error::HelixError;

/// How this connection is to be protected.
///
/// Deliberately two values where `libpq` has six. `prefer` (its default, and Go's) lets the
/// SERVER decide whether the session is encrypted; `require` encrypts without checking who
/// answered; `verify-ca` checks the chain but not the name. Each is a trap with a name, and
/// each is refused below with the reason. See `pg::tls` for the whole argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslMode {
    /// TLS, chain verified to a trusted root, certificate matched against the host.
    VerifyFull,
    /// Plaintext, by the caller's explicit request.
    Disable,
}

/// How long a statement may take — the URL's `timeout=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Patience {
    /// The URL said nothing. A statement may run as long as it likes, but the server may not
    /// be SILENT for more than thirty seconds — what this client has always done, because a
    /// program that waits forever on a server that has stopped answering cannot be
    /// interrupted from inside.
    Silence,
    /// `timeout=N`: the SERVER ends a statement that runs past this (see `connect`).
    Limit(Duration),
    /// `timeout=0`: as long as it takes.
    Unbounded,
}

/// Where to connect and as whom, parsed from a URL.
#[derive(Debug)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// How the connection is protected. `VerifyFull` unless the URL says otherwise, and
    /// nothing on the network can change it — see `tls` for why that is the whole point.
    pub sslmode: SslMode,
    /// A PEM file that REPLACES the default anchors, for a private or provider CA.
    pub sslrootcert: Option<String>,
    /// How long a statement may take (`timeout=`, in seconds).
    pub patience: Patience,
    /// How long to wait for the TCP connection (`connect_timeout=`, in seconds; 10).
    pub connect_timeout: Duration,
}

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The server takes `statement_timeout` as a 32-bit count of milliseconds, so this many
/// seconds — a little under 25 days — is as far as a limit goes. Longer than that is
/// `timeout=0`.
const MAX_TIMEOUT_SECS: u64 = 2_147_483;

/// Parse `postgres://user:pass@host:port/database`.
///
/// The `url` crate does this, and it is a direct dependency for exactly this reason: a
/// hand-rolled scheme/host/port split is where credential-handling bugs live, as the
/// comment on that dependency already says for the HTTP client.
pub fn parse_url(raw: &str, line: usize, col: usize) -> Result<Target, HelixError> {
    let bad = |m: String| HelixError::new(m, line, col).hint(
        "a connection URL looks like `postgres://user:password@host:5432/database`.",
    );
    // THE URL IS NOT ECHOED, because it carries a password. An error message is the most
    // widely copied text a program produces — printed, logged, pasted into an issue — and
    // a credential that reaches one has escaped. The caller wrote this URL and the line and
    // column already point at the call, so quoting it back adds nothing they do not have.
    let u = url::Url::parse(raw)
        .map_err(|e| bad(format!("the connection URL could not be parsed: {e}")))?;
    if !matches!(u.scheme(), "postgres" | "postgresql") {
        return Err(bad(format!(
            "`{}` is not a PostgreSQL URL scheme — expected `postgres://` or `postgresql://`",
            u.scheme()
        )));
    }
    let host = u.host_str().ok_or_else(|| bad("the URL has no host".into()))?.to_string();
    let user = match u.username() {
        "" => return Err(bad("the URL has no user".into())),
        s => percent_decode(s),
    };
    let database = u.path().trim_start_matches('/').to_string();
    if database.is_empty() {
        return Err(bad("the URL has no database — it is the path, as in `/mydb`".into()));
    }
    // The connection parameters. Unknown keys and unknown values are ERRORS, not
    // ignored: `sslmode=requrie` silently meaning "the default" is exactly how the
    // capability sandbox once failed open on a typo, and this is the same shape of
    // mistake with the same consequence.
    let mut sslmode = SslMode::VerifyFull;
    let mut sslrootcert: Option<String> = None;
    let mut patience = Patience::Silence;
    let mut connect_timeout = DEFAULT_CONNECT_TIMEOUT;
    // A number of whole seconds, or an error that says what one looks like.
    let seconds = |key: &str, v: &str| -> Result<u64, HelixError> {
        match v.parse::<u64>() {
            Ok(n) if n <= MAX_TIMEOUT_SECS => Ok(n),
            Ok(_) => Err(bad(format!(
                "`{key}={v}` is longer than the server can count — the most is {MAX_TIMEOUT_SECS} seconds"
            ))
            .hint("`timeout=0` is no limit at all.")),
            Err(_) => Err(bad(format!("`{key}={v}` is not a number of seconds"))
                .hint("e.g. `timeout=300` lets a statement run five minutes; `timeout=0` is no limit.")),
        }
    };
    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "timeout" => {
                patience = match seconds("timeout", v.as_ref())? {
                    0 => Patience::Unbounded,
                    n => Patience::Limit(Duration::from_secs(n)),
                }
            }
            "connect_timeout" => {
                connect_timeout = match seconds("connect_timeout", v.as_ref())? {
                    0 => {
                        return Err(bad("`connect_timeout=0` would never give up on a host that is not there".to_string())
                            .hint("it is a number of seconds, at least 1; the default is 10."))
                    }
                    n => Duration::from_secs(n),
                }
            }
            "sslmode" => {
                sslmode = match v.as_ref() {
                    "verify-full" => SslMode::VerifyFull,
                    "disable" => SslMode::Disable,
                    // Each refusal names what the mode would have cost, because "not
                    // supported" is not an answer someone can act on.
                    "prefer" | "allow" => {
                        return Err(bad(format!(
                            "`sslmode={v}` lets the SERVER decide whether your connection is \
                             encrypted — an attacker on the path just answers \"no TLS\" and \
                             reads the password exchange"
                        ))
                        .hint(
                            "leave `sslmode` out for verified TLS, or say `sslmode=disable` \
                             to ask for plaintext on purpose.",
                        ))
                    }
                    "require" => {
                        return Err(bad(
                            "`sslmode=require` encrypts without checking who you are talking \
                             to, so anyone who can answer on that port can present any \
                             certificate and be believed"
                                .to_string(),
                        )
                        .hint(
                            "leave `sslmode` out for verified TLS; for a private or provider \
                             CA add `sslrootcert=/path/to/ca.pem`.",
                        ))
                    }
                    "verify-ca" => {
                        return Err(bad(
                            "`sslmode=verify-ca` checks the certificate chain but not the \
                             hostname, so a valid certificate for another host passes"
                                .to_string(),
                        )
                        .hint("leave `sslmode` out — verified TLS is the default."))
                    }
                    other => {
                        return Err(bad(format!("`sslmode={other}` is not a value")).hint(
                            "Helix takes `verify-full` (the default) or `disable`.",
                        ))
                    }
                }
            }
            "sslrootcert" => sslrootcert = Some(v.into_owned()),
            other => {
                return Err(bad(format!(
                    "`{other}` is not a connection parameter Helix understands"
                ))
                .hint("the URL takes `sslmode`, `sslrootcert`, `timeout` and `connect_timeout`."))
            }
        }
    }
    if sslmode == SslMode::Disable && sslrootcert.is_some() {
        return Err(bad(
            "`sslrootcert` names the certificate to trust, and `sslmode=disable` asks for \
             no certificate at all — the URL is asking for two different things"
                .to_string(),
        )
        .hint("drop one of them."));
    }
    Ok(Target {
        host,
        port: u.port().unwrap_or(5432),
        user,
        password: u.password().map(percent_decode).unwrap_or_default(),
        database,
        sslmode,
        sslrootcert,
        patience,
        connect_timeout,
    })
}

/// Percent-decoding for the credential fields, which is where `%40` for `@` shows up.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ok(url: &str) -> Target {
        parse_url(url, 1, 1).expect(url)
    }
    fn err(url: &str) -> String {
        let e = parse_url(url, 1, 1).expect_err(url);
        format!("{} {}", e.message, e.hint.clone().unwrap_or_default())
    }

    /// THE SECURITY POLICY, pinned. `sslmode`'s accepted values are the whole of it: which
    /// spellings mean encrypted, which mean plaintext, and which are refused because they
    /// let something other than the caller decide.
    ///
    /// These live here, outside `#[cfg(feature = "postgres")]`, because the gate does not
    /// build that feature — a policy test inside it would be a test that never runs.
    #[test]
    fn verified_tls_is_the_default_and_only_the_url_can_change_it() {
        // Writing nothing gets you verified TLS. This is the line the whole design turns
        // on: `libpq` and Go's `pgx` default to `prefer`, which is plaintext whenever the
        // server says so.
        let t = ok("postgres://u:p@h:5432/db");
        assert_eq!(t.sslmode, SslMode::VerifyFull);
        assert_eq!(t.sslrootcert, None);
        assert_eq!(ok("postgres://u:p@h/db?sslmode=verify-full").sslmode, SslMode::VerifyFull);

        // Plaintext exists, spelled out by the person who wants it.
        assert_eq!(ok("postgres://u:p@h/db?sslmode=disable").sslmode, SslMode::Disable);

        // A private or provider CA is a FILE, never a switch that turns checking off.
        let t = ok("postgres://u:p@h/db?sslrootcert=/etc/ssl/rds.pem");
        assert_eq!(t.sslrootcert.as_deref(), Some("/etc/ssl/rds.pem"));
        assert_eq!(t.sslmode, SslMode::VerifyFull);

        // Each refused mode names what it would have cost. "Not supported" is not an
        // answer anyone can act on.
        for (mode, needle) in [
            ("prefer", "lets the SERVER decide"),
            ("allow", "lets the SERVER decide"),
            ("require", "without checking who you are talking to"),
            ("verify-ca", "not the hostname"),
        ] {
            let m = err(&format!("postgres://u:p@h/db?sslmode={mode}"));
            assert!(m.contains(needle), "`{mode}` said: {m}");
        }

        // A TYPO IS AN ERROR, not the default. The capability sandbox once failed open on
        // exactly this shape of mistake, and silently meaning `verify-full` here would be
        // the benign twin of silently meaning `prefer`.
        assert!(err("postgres://u:p@h/db?sslmode=requrie").contains("is not a value"));
        // So is a parameter this client cannot honour: accepting `sslcompression` by
        // ignoring it would be a claim that it was applied.
        assert!(err("postgres://u:p@h/db?sslcompression=1").contains("not a connection parameter"));

        // Two requests that contradict each other are refused rather than ranked.
        let m = err("postgres://u:p@h/db?sslmode=disable&sslrootcert=/x.pem");
        assert!(m.contains("two different things"), "{m}");
    }

    /// HOW LONG A STATEMENT MAY TAKE IS THE URL'S TO SAY. Saying nothing changes nothing; a
    /// number of seconds is a limit the server enforces; `0` is as long as it takes. A value
    /// that is not a number is an error, like every other parameter this client cannot honour
    /// exactly.
    #[test]
    fn the_url_says_how_long_a_statement_may_take() {
        let t = ok("postgres://u:p@h/db");
        assert_eq!((t.patience, t.connect_timeout), (Patience::Silence, Duration::from_secs(10)));
        assert_eq!(ok("postgres://u:p@h/db?timeout=300").patience, Patience::Limit(Duration::from_secs(300)));
        assert_eq!(ok("postgres://u:p@h/db?timeout=0").patience, Patience::Unbounded);
        assert_eq!(ok("postgres://u:p@h/db?connect_timeout=3&sslmode=disable").connect_timeout, Duration::from_secs(3));
        assert!(err("postgres://u:p@h/db?timeout=5m").contains("`timeout=5m` is not a number of seconds"));
        assert!(err("postgres://u:p@h/db?timeout=-1").contains("is not a number of seconds"));
        assert!(err("postgres://u:p@h/db?timeout=99999999").contains("longer than the server can count"));
        assert!(err("postgres://u:p@h/db?connect_timeout=0").contains("would never give up"));
        assert!(err("postgres://u:p@h/db?nope=1").contains("`timeout`"), "the hint names every parameter");
    }

    /// A password must never reach an error message. Errors are the most widely copied
    /// text a program produces, and a credential that reaches one has escaped.
    #[test]
    fn the_password_never_appears_in_an_error() {
        const PW: &str = "hunter2-s3cret";
        for url in [
            // Unparseable — this is the one that used to echo the whole URL back.
            &format!("postgres://u:{PW}@h:notaport/db"),
            &format!("ldap://u:{PW}@h/db"),
            &format!("postgres://u:{PW}@h/db?sslmode=prefer"),
            &format!("postgres://u:{PW}@h/db?sslmode=nonsense"),
            &format!("postgres://u:{PW}@h/db?nope=1"),
            &format!("postgres://u:{PW}@h/"),
        ] {
            let m = err(url);
            assert!(!m.contains(PW), "the password leaked into: {m}");
        }
    }

    /// The credential fields are percent-decoded, which is how a password containing `@`
    /// or `/` survives being in a URL at all.
    #[test]
    fn credentials_are_percent_decoded() {
        let t = ok("postgres://us%40er:p%2Fss%3Aword@h:5432/db");
        assert_eq!(t.user, "us@er");
        assert_eq!(t.password, "p/ss:word");
        assert_eq!(t.host, "h");
        assert_eq!(t.port, 5432);
        assert_eq!(t.database, "db");
    }
}
