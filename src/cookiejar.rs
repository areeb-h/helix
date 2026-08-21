//! The cookie jar (ADR 0031 §4): storage and send policy on top of the wire-format
//! parsers in `interp/builtins.rs`. The part that decides whether a cookie may be
//! *stored*, and whether it may be *sent*.
//!
//! The jar is EXPLICIT state a program passes, never ambient — a value the program
//! holds and threads into each request. ADR 0020's reasoning applies: an implicit,
//! process-wide jar would make the second run of a program differ from the first, and
//! reproducibility is the house's flagship claim. Here the jar is a `RefCell<Vec<…>>`
//! behind a handle the program owns.
//!
//! The security core is two rules, each a documented attack when it is missing:
//!
//!   * a cookie's `Domain` must be a suffix of the request host (RFC 6265 §5.1.3), and
//!   * that `Domain` must NOT be a public suffix — the supercookie defence. Without the
//!     Public Suffix List, `evil.co.uk` could set a cookie for `.co.uk`, readable by
//!     every site under that suffix. The `psl` crate carries Mozilla's list, so this is
//!     a real check rather than a hopeful one.

use std::cell::RefCell;

/// A cookie the jar has accepted, with the scope that decides where it is sent.
#[derive(Clone, Debug)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    /// The effective domain. `host_only` true means it is sent ONLY to exactly this
    /// host (no `Domain` attribute was set, or one was rejected); false means this host
    /// and its subdomains (a `Domain` attribute that passed the two rules above).
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    /// Absolute expiry as a Unix second, or `None` for a session cookie (kept until the
    /// jar is dropped). Computed from `Max-Age` (preferred) or `Expires` at store time.
    pub expires_at: Option<i64>,
}

/// A per-domain and total cap (ADR 0024): a `Set-Cookie` avalanche must not grow the
/// jar without bound. RFC 6265 §6.1 suggests 50 per domain / 3000 total; these match.
const MAX_PER_DOMAIN: usize = 50;
const MAX_TOTAL: usize = 3000;

/// The jar itself — interior-mutable, because a request mutates it (stores what a
/// response set) while the program holds it by shared reference.
#[derive(Default)]
pub struct CookieJar {
    cookies: RefCell<Vec<StoredCookie>>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cookies.borrow().len()
    }

    pub fn clear(&self) {
        self.cookies.borrow_mut().clear();
    }

    /// A snapshot of the live cookies as `(name, value, domain, path)`, for a program
    /// that wants to inspect the jar. Evicts expired entries first, so a caller never
    /// sees a cookie that would not be sent.
    pub fn snapshot(&self, now: i64) -> Vec<(String, String, String, String)> {
        self.evict_expired(now);
        self.cookies
            .borrow()
            .iter()
            .map(|c| (c.name.clone(), c.value.clone(), c.domain.clone(), c.path.clone()))
            .collect()
    }

    fn evict_expired(&self, now: i64) {
        self.cookies
            .borrow_mut()
            .retain(|c| c.expires_at.is_none_or(|t| t > now));
    }

    /// Store a cookie parsed from a `Set-Cookie` header received from `request_host`,
    /// applying the storage policy. Returns `Ok(true)` if stored, `Ok(false)` if
    /// rejected by policy (a rejection is not an error — a server may set a cookie it is
    /// not allowed to, and the right response is to drop it, not to fail the request).
    ///
    /// `attrs` are the parsed `Set-Cookie` attributes; `now` is the current Unix second.
    #[allow(clippy::too_many_arguments)]
    pub fn store(
        &self,
        request_host: &str,
        name: String,
        value: String,
        domain_attr: Option<&str>,
        path_attr: Option<&str>,
        secure: bool,
        max_age: Option<i64>,
        expires_raw: Option<&str>,
        now: i64,
    ) -> bool {
        let host = request_host.trim_start_matches('.').to_ascii_lowercase();

        // Domain scope. No `Domain` (or an empty one) → host-only: sent to exactly this
        // host. A `Domain` must pass BOTH rules to widen scope to subdomains.
        let (domain, host_only) = match domain_attr.map(|d| d.trim_start_matches('.').to_ascii_lowercase()) {
            None => (host.clone(), true),
            Some(d) if d.is_empty() => (host.clone(), true),
            Some(d) => {
                // Rule 1: the Domain must be the host or a parent of it.
                let is_suffix = host == d || host.ends_with(&format!(".{d}"));
                // Rule 2 (the supercookie defence): the Domain must not itself be a
                // public suffix. `.co.uk`, `.github.io`, `.s3.amazonaws.com` — a cookie
                // scoped to one of these would be readable across unrelated sites.
                let is_public = is_public_suffix(&d);
                if !is_suffix || is_public {
                    // Rejected as a wider scope; fall back to host-only, which is always
                    // safe, rather than dropping the cookie entirely.
                    (host.clone(), true)
                } else {
                    (d, false)
                }
            }
        };

        let path = match path_attr {
            Some(p) if p.starts_with('/') => p.to_string(),
            // RFC 6265 §5.1.4 default-path is the request path's directory; we do not
            // have the request path here, so default to "/", the safe broad default a
            // server usually intends when it omits Path.
            _ => "/".to_string(),
        };

        // Max-Age wins over Expires (RFC 6265 §5.2.2). A non-positive Max-Age is an
        // immediate-expiry delete.
        let expires_at = match max_age {
            Some(secs) => Some(now + secs),
            None => expires_raw.and_then(parse_http_date),
        };
        if expires_at.is_some_and(|t| t <= now) {
            // A delete: remove any matching cookie and store nothing.
            self.cookies
                .borrow_mut()
                .retain(|c| !(c.name == name && c.domain == domain && c.path == path));
            return true;
        }

        let cookie = StoredCookie {
            name,
            value,
            domain,
            host_only,
            path,
            secure,
            expires_at,
        };

        self.evict_expired(now);
        let mut jar = self.cookies.borrow_mut();
        // Upsert: a later Set-Cookie for the same (name, domain, path) replaces.
        if let Some(slot) = jar
            .iter_mut()
            .find(|c| c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        {
            *slot = cookie;
            return true;
        }
        // Caps (ADR 0024): evict the oldest of this domain, then the oldest overall.
        if jar.iter().filter(|c| c.domain == cookie.domain).count() >= MAX_PER_DOMAIN
            && let Some(pos) = jar.iter().position(|c| c.domain == cookie.domain)
        {
            jar.remove(pos);
        }
        if jar.len() >= MAX_TOTAL {
            jar.remove(0);
        }
        jar.push(cookie);
        true
    }

    /// The `Cookie:` header value to send with a request to `url`, or `None` if no
    /// stored cookie matches. Honours host/path scope and `Secure` (never over http),
    /// and evicts expired cookies as it reads (ADR 0031: eviction on read as well as
    /// write, so a long-lived jar cannot accumulate the dead).
    pub fn cookie_header(&self, url: &url::Url, now: i64) -> Option<String> {
        self.evict_expired(now);
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let is_https = url.scheme() == "https";

        let jar = self.cookies.borrow();
        let mut matched: Vec<&StoredCookie> = jar
            .iter()
            .filter(|c| {
                // Secure cookies never travel over plain http.
                if c.secure && !is_https {
                    return false;
                }
                // Host scope.
                let host_ok = if c.host_only {
                    host == c.domain
                } else {
                    host == c.domain || host.ends_with(&format!(".{}", c.domain))
                };
                if !host_ok {
                    return false;
                }
                // Path scope (RFC 6265 §5.1.4): the request path is the cookie path, or
                // under it at a `/` boundary.
                path == c.path
                    || (path.starts_with(&c.path)
                        && (c.path.ends_with('/') || path[c.path.len()..].starts_with('/')))
            })
            .collect();
        if matched.is_empty() {
            return None;
        }
        // Longer paths first (more specific), then insertion order — RFC 6265 §5.4.
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            matched
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// The fields of one `Set-Cookie` header, parsed once. Shared by the jar (which stores
/// them) and the `parse_set_cookie` builtin (which surfaces them), so the two cannot
/// disagree about what a header meant. Attribute names are matched case-insensitively
/// (RFC 6265 §5.2); `Secure`/`HttpOnly` are valueless flags; `Expires` is kept raw
/// because a server that cares about expiry sends `Max-Age`, which wins over it.
#[derive(Clone, Debug, Default)]
pub struct SetCookie {
    pub name: String,
    pub value: String,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
    pub max_age: Option<i64>,
    pub expires: Option<String>,
}

/// Parse a `Set-Cookie` value. `None` if there is no `name=value` before the first `;` —
/// that is not a cookie.
pub fn parse_set_cookie(header: &str) -> Option<SetCookie> {
    let mut parts = header.split(';');
    let first = parts.next().unwrap_or("").trim();
    let (name, value) = first.split_once('=')?;
    let mut c = SetCookie {
        name: name.trim().to_string(),
        value: value.trim().trim_matches('"').to_string(),
        ..Default::default()
    };
    if c.name.is_empty() {
        return None;
    }
    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (attr.to_ascii_lowercase(), String::new()),
        };
        match k.as_str() {
            "secure" => c.secure = true,
            "httponly" => c.http_only = true,
            "path" => c.path = Some(v),
            "domain" => c.domain = Some(v),
            "expires" => c.expires = Some(v),
            "samesite" => c.same_site = Some(v),
            "max-age" => c.max_age = v.trim().parse().ok(),
            _ => {}
        }
    }
    Some(c)
}

impl CookieJar {
    /// Parse a `Set-Cookie` header received from `request_host` and store it under the
    /// jar's policy. A header that is not a cookie, or one policy rejects, is simply not
    /// stored — a response setting a cookie it may not is dropped, never an error.
    pub fn store_from_header(&self, request_host: &str, header: &str, now: i64) {
        if let Some(c) = parse_set_cookie(header) {
            self.store(
                request_host,
                c.name,
                c.value,
                c.domain.as_deref(),
                c.path.as_deref(),
                c.secure,
                c.max_age,
                c.expires.as_deref(),
                now,
            );
        }
    }
}

/// Is `domain` a public suffix (a registry under which anyone may register, like
/// `co.uk` or `github.io`)? A cookie may never be scoped to one — that is the
/// supercookie. Answered by the `psl` crate's embedded Mozilla list.
///
/// An unknown TLD (`psl` cannot classify it) is treated as a public suffix — the
/// conservative choice the ADR names: refusing to widen a cookie's scope is always
/// safe, and the cost is only that a cookie on an exotic TLD stays host-only.
fn is_public_suffix(domain: &str) -> bool {
    use psl::Psl;
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    match psl::List.suffix(d.as_bytes()) {
        // The whole name IS the suffix → a public suffix, reject.
        Some(suffix) => suffix.as_bytes() == d.as_bytes(),
        // Unclassifiable → conservative: treat as public (host-only fallback).
        None => true,
    }
}


/// The current time as a Unix second. `SystemTime`, so no crate is pulled in for a
/// clock; a pre-1970 system clock (impossible in practice) reads as 0.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an HTTP `Expires` date (RFC 7231 IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`)
/// to a Unix second. `None` if it does not match — the caller then treats the cookie as
/// a session cookie, which is safe: a session cookie is dropped when the jar is, never
/// persisted past a lifetime it might have named.
///
/// Only the one RFC-MANDATED format is accepted. The two obsolete formats (RFC 850,
/// asctime) are not worth the surface; a server that cares about expiry sends Max-Age,
/// which wins over this anyway.
fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let rest = s.split_once(", ")?.1; // drop the weekday
    let mut it = rest.split_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let month = match it.next()? {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next()?.parse().ok()?;
    // Days from the Unix epoch to `year-month-day` via the civil-from-days algorithm
    // (Howard Hinnant), run forward.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12; // Mar=0..Feb=11
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468; // 0000-03-01 .. 1970-01-01
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_epochs() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Wed, 09 Jun 2027 10:18:14 GMT"), Some(1812536294));
        assert_eq!(parse_http_date("not a date"), None);
    }

    #[test]
    fn store_refuses_a_supercookie_but_keeps_a_real_domain_cookie() {
        // The sharp case: the host is UNDER a public suffix, so Domain=.co.uk IS a valid
        // suffix of it (rule 1 passes) and only the PSL check (rule 2) can stop it.
        let jar = CookieJar::new();
        // A supercookie attempt scoped to the registry co.uk -> rejected -> host-only.
        jar.store("shop.example.co.uk", "a".into(), "1".into(), Some(".co.uk"), None, false, None, None, 0);
        // A legitimate domain cookie scoped to the site -> accepted -> domain cookie.
        jar.store("shop.example.co.uk", "b".into(), "2".into(), Some(".example.co.uk"), None, false, None, None, 0);
        let cs = jar.snapshot(0);
        let a = cs.iter().find(|(n, ..)| n == "a").unwrap();
        let b = cs.iter().find(|(n, ..)| n == "b").unwrap();
        assert_eq!(a.2, "shop.example.co.uk", "a supercookie was accepted at co.uk scope");
        assert_eq!(b.2, "example.co.uk", "a legitimate domain cookie was narrowed");
    }

    #[test]
    fn a_public_suffix_domain_is_rejected() {
        assert!(is_public_suffix("co.uk"));
        assert!(is_public_suffix("github.io"));
        assert!(!is_public_suffix("example.co.uk"));
        assert!(!is_public_suffix("example.com"));
    }
}
