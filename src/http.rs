//! A minimal HTTP client for fetching data from a Helix program — the common
//! scientific "pull data from a URL / REST API" need. `http_get(url)` returns a
//! record `{status, body}`; the body is typically fed to `parse_json`. Behind the
//! default-on `http` feature (build `--no-default-features` for a network-free
//! binary). HTTPS is preferred; an HTTP error status is returned as data (so a 404
//! is a `status`, not a crash), while transport failures are Helix errors.

/// A fetched response: `(status, body, response_headers)`. `get`/`post` ignore the headers
/// (returning `(status, body)`); the general `request` returns all three.
#[cfg(feature = "http")]
pub type Fetched = (i64, String, Vec<(String, String)>);

/// A `request` result: the response plus the chain of URLs redirected THROUGH (empty
/// when there were none). Returned as data because a chain followed silently is a
/// chain nobody audited (ADR 0031).
#[cfg(feature = "http")]
pub type FetchedWithChain = (i64, String, Vec<(String, String)>, Vec<String>);

/// The process-wide client, built once.
///
/// A `ureq::Agent` IS the connection pool: it holds the keep-alive connections and the
/// TLS sessions. Building one per call — which every function here used to do — opens a
/// fresh TCP connection and redoes the TLS handshake for every request, then discards
/// it. For the shape this client is actually used in (a loop against one API host) that
/// handshake dominates the request, so the pool is the difference between talking to a
/// server and re-introducing yourself to it every time.
///
/// Cloning an `Agent` is cheap and shares the same pool, which is how ureq intends it to
/// be used across threads.
/// Per-request limits (ADR 0031 §3). Every field optional; the defaults are the
/// values the client has always used, so a request that sets nothing changes nothing.
#[cfg(feature = "http")]
#[derive(Clone, Copy, Default)]
pub struct Limits {
    /// Overall deadline for the whole request–response exchange. The one that bounds
    /// a hostile server: a slow-loris keeps each individual read inside the read
    /// timeout, and only a total deadline ends it.
    pub total_ms: Option<u64>,
    /// TCP connect timeout (default 30s). Setting this (or `read_ms`) builds a
    /// dedicated agent for the request — tuning connection behaviour opts that one
    /// call out of the shared pool, which is the honest cost of a non-default socket.
    pub connect_ms: Option<u64>,
    /// Per-read socket timeout (default 120s).
    pub read_ms: Option<u64>,
    /// Response-body cap in bytes (default 10 MiB — the cap that already existed,
    /// now with a name). Hitting it is an error naming the cap, never a truncated
    /// body under a 200: a wrong body that parses is worse than a failure.
    pub max_body: Option<usize>,
}

#[cfg(feature = "http")]
const DEFAULT_MAX_BODY: usize = 10 * 1024 * 1024;

/// Response headers are attacker-controlled input too (ADR 0024): a header-bombing
/// server must produce an error, not an unbounded Vec.
#[cfg(feature = "http")]
const MAX_RESPONSE_HEADERS: usize = 512;

/// The agent for a request with these limits: the shared pool unless the request
/// tunes socket behaviour, which ureq 2 only supports agent-wide.
#[cfg(feature = "http")]
fn agent_for(limits: &Limits) -> ureq::Agent {
    if limits.connect_ms.is_none() && limits.read_ms.is_none() {
        return agent();
    }
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_millis(limits.connect_ms.unwrap_or(30_000)))
        .timeout_read(std::time::Duration::from_millis(limits.read_ms.unwrap_or(120_000)))
        .redirects(0)
        .build()
}

/// Read a response body to `max_body` bytes. One byte more is an ERROR that names the
/// limit and the field that raises it — the body is attacker-sized, and handing back a
/// prefix as if it were the whole is the silent-wrong-answer class.
#[cfg(feature = "http")]
fn read_body(resp: ureq::Response, max_body: usize) -> Result<String, String> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    resp.into_reader()
        .take(max_body as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("reading the response body failed: {e}"))?;
    if buf.len() > max_body {
        return Err(format!(
            "the response body exceeds max_body ({max_body} bytes) — raise `max_body` in the request record, or stream it with `http_stream`"
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| "the response body is not valid UTF-8 text".to_string())
}

#[cfg(feature = "http")]
fn agent() -> ureq::Agent {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            // Connect/read timeouts so a hung or slow-loris server cannot stall the
            // program indefinitely. (`into_string` already caps a body at ureq's 10 MiB.)
            ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(30))
                .timeout_read(std::time::Duration::from_secs(120))
                // We follow redirects ourselves (see `follow`), applying the boundary
                // rules ureq cannot express. ureq must never follow one we have not
                // vetted, so it returns every 3xx to us.
                .redirects(0)
                .build()
        })
        .clone()
}

#[cfg(feature = "http")]
pub fn get(url: &str) -> Result<(i64, String), String> {
    let ((status, body, _), _) = follow("GET", url, "", &[], &Limits::default(), None)?;
    Ok((status, body))
}

/// `http_post(url, body)` — send `body` with the given `content_type` and return
/// `{status, body}`, mirroring [`get`]: a non-2xx status is data (so a REST 400/422 is a
/// `status`, not a crash), transport failures are errors. `body` is sent verbatim — the
/// caller builds it (typically `record.to_json()` for a JSON API, the default content type).
#[cfg(feature = "http")]
pub fn post(url: &str, body: &str, content_type: &str) -> Result<(i64, String), String> {
    let headers = [("Content-Type".to_string(), content_type.to_string())];
    let ((status, rbody, _), _) = follow("POST", url, body, &headers, &Limits::default(), None)?;
    Ok((status, rbody))
}

/// The redirect chain cap (ADR 0031). A loop, or a server pointing a client in a
/// circle, must end rather than run forever (ADR 0024).
#[cfg(feature = "http")]
const MAX_REDIRECTS: usize = 10;

/// Did the origin change between two URLs — scheme, host, or port? The test that
/// decides whether credentials may cross. Parsed by the `url` crate, not by hand,
/// because a wrong answer here leaks a bearer token to another host.
#[cfg(feature = "http")]
fn origin_changed(from: &url::Url, to: &url::Url) -> bool {
    from.scheme() != to.scheme()
        || from.host_str() != to.host_str()
        || from.port_or_known_default() != to.port_or_known_default()
}

/// The method and body a redirect continues with (RFC 9110 §15.4, RFC 10008 §3).
///
/// Returns `(new_method, keep_body)`. 303 means "retrieve the result with GET", so it
/// becomes GET and drops the body for every method but HEAD. 301/302 keep the method,
/// EXCEPT the historical POST -> GET — which RFC 10008 excludes QUERY from, so a QUERY
/// stays a QUERY with its body. 307/308 preserve method and body for everything.
#[cfg(feature = "http")]
fn redirect_method(status: i64, method: &str) -> (&str, bool) {
    match status {
        303 => {
            if method.eq_ignore_ascii_case("HEAD") {
                ("HEAD", false)
            } else {
                ("GET", false)
            }
        }
        301 | 302 => {
            if method.eq_ignore_ascii_case("POST") {
                ("GET", false)
            } else {
                // GET/HEAD keep themselves; QUERY keeps itself AND its body (RFC 10008).
                (method, !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD"))
            }
        }
        // 307 / 308: preserve everything.
        _ => (method, true),
    }
}

/// Send one request and follow redirects under the ADR 0031 boundary rules, returning
/// the final response and the chain of URLs that led to it. `get`/`post`/`request` all
/// route through here, so the safe policy is uniform and the pool is shared.
#[cfg(feature = "http")]
fn follow(
    method: &str,
    url_str: &str,
    body: &str,
    headers: &[(String, String)],
    limits: &Limits,
    jar: Option<&crate::cookiejar::CookieJar>,
) -> Result<(Fetched, Vec<String>), String> {
    let mut cur_url: url::Url = url_str
        .parse()
        .map_err(|e| format!("`{url_str}` is not a valid URL: {e}"))?;
    let mut cur_method = method.to_string();
    let mut cur_body = body.to_string();
    // The headers carried forward. Cloned once; a cross-origin hop drops the
    // credential-bearing ones from THIS copy, never the caller's record.
    let mut cur_headers: Vec<(String, String)> = headers.to_vec();
    let mut chain: Vec<String> = Vec::new();

    loop {
        let mut req = agent_for(limits).request(cur_method.as_str(), cur_url.as_str());
        if let Some(ms) = limits.total_ms {
            req = req.timeout(std::time::Duration::from_millis(ms));
        }
        for (k, v) in &cur_headers {
            req = req.set(k, v);
        }
        // The jar's cookies for THIS hop's URL — recomputed per hop, so a redirect to a
        // new host sends that host's cookies, not the previous one's (the cross-origin
        // header strip above removed the old Cookie header; this adds the right one).
        let now = crate::cookiejar::now_unix();
        if let Some(j) = jar
            && let Some(cookie) = j.cookie_header(&cur_url, now)
        {
            req = req.set("Cookie", &cookie);
        }
        let send = if cur_body.is_empty() { req.call() } else { req.send_string(&cur_body) };

        // ureq returns a 3xx as `Ok` (only 4xx/5xx are `Err::Status`), so the response
        // comes out here whether or not it redirects; a genuine transport failure is
        // the only `Err`.
        let resp = match send {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => {
                if limits.total_ms.is_some() && format!("{e}").contains("timed out") {
                    return Err(format!(
                        "HTTP {cur_method} to {cur_url} exceeded total_ms ({} ms): {e}",
                        limits.total_ms.unwrap_or(0)
                    ));
                }
                return Err(format!("HTTP {cur_method} to {cur_url} failed: {e}"));
            }
        };
        let code = resp.status() as i64;
        // A 3xx with a `Location` is a redirect to vet and follow; anything else is the
        // final response, returned as data.
        if (300..400).contains(&code)
            && let Some(location) = resp.header("location").map(str::to_string)
        {
                if chain.len() >= MAX_REDIRECTS {
                    return Err(format!(
                        "more than {MAX_REDIRECTS} redirects starting from {url_str} — a redirect loop, or a server pointing in a circle"
                    ));
                }
                let next: url::Url = cur_url.join(&location).map_err(|e| {
                    format!("redirect target `{location}` from {cur_url} is not a valid URL: {e}")
                })?;
                // Scheme gate: only http/https, and never a downgrade.
                match next.scheme() {
                    "https" => {}
                    "http" => {
                        if cur_url.scheme() == "https" {
                            return Err(format!(
                                "refused an https -> http downgrade redirect from {cur_url} to {next}"
                            ));
                        }
                    }
                    other => {
                        return Err(format!(
                            "refused a redirect to a `{other}:` URL ({next}) — only http and https are followed"
                        ))
                    }
                }
                // Credential headers do not cross an origin boundary.
                if origin_changed(&cur_url, &next) {
                    cur_headers.retain(|(k, _)| {
                        !k.eq_ignore_ascii_case("authorization")
                            && !k.eq_ignore_ascii_case("cookie")
                            && !k.eq_ignore_ascii_case("proxy-authorization")
                    });
                }
                // A redirect can set cookies too — a login 302 sets the session
                // cookie — so store from this response before following.
                if let Some(j) = jar {
                    let host = cur_url.host_str().unwrap_or("");
                    for name in resp.headers_names() {
                        if name.eq_ignore_ascii_case("set-cookie") {
                            for v in resp.all(&name) {
                                j.store_from_header(host, v, now);
                            }
                        }
                    }
                }
                let (m, keep_body) = redirect_method(code, &cur_method);
                cur_method = m.to_string();
                if !keep_body {
                    cur_body.clear();
                }
                chain.push(cur_url.to_string());
                cur_url = next;
                continue;
            // A 3xx with no `Location` is not a redirect we can follow — it falls
            // through and is returned as the data it is.
        }
        let fetched = collect_response(resp, limits.max_body.unwrap_or(DEFAULT_MAX_BODY))
            .map_err(|e| format!("HTTP {cur_method} to {cur_url}: {e}"))?;
        // Store what this response set. `fetched.2` is the response headers in order, so
        // repeated `Set-Cookie`s are all seen.
        if let Some(j) = jar {
            let host = cur_url.host_str().unwrap_or("");
            for (k, v) in &fetched.2 {
                if k.eq_ignore_ascii_case("set-cookie") {
                    j.store_from_header(host, v, now);
                }
            }
        }
        return Ok((fetched, chain));
    }
}

/// The general client primitive behind `http_request({method, url, body, headers})`: any
/// method, caller-supplied request `headers`, and — unlike get/post — the **response
/// headers** are returned too. `body` is sent verbatim (empty → no request body). A non-2xx
/// status is data (status + body + headers), transport failures are errors.
#[cfg(feature = "http")]
pub fn request(
    method: &str,
    url: &str,
    body: &str,
    headers: &[(String, String)],
    limits: &Limits,
    jar: Option<&crate::cookiejar::CookieJar>,
) -> Result<FetchedWithChain, String> {
    let ((status, body, hs), chain) = follow(method, url, body, headers, limits, jar)?;
    Ok((status, body, hs, chain))
}

/// Open a request for **streaming**: return `(status, body_reader)` without reading the body
/// up front (unlike get/post/request), so the caller pulls it line-by-line. Powers
/// `http_stream` for token-by-token model output.
///
/// `timeout_ms` bounds the wait for *each* chunk (via the socket read timeout, `SO_RCVTIMEO` —
/// applied per read, so it's a per-chunk deadline, not a whole-stream one). `None` means no
/// read timeout: a streaming response may idle between chunks (a model thinking) far longer
/// than a normal body read, and the program decides when to stop. `Some(ms)` lets a caller
/// tell a genuinely hung server from a merely slow one — a chunk that doesn't arrive in time
/// surfaces as a (recoverable) read timeout on `.next()`.
#[cfg(feature = "http")]
pub fn open_stream(
    method: &str,
    url: &str,
    body: &str,
    headers: &[(String, String)],
    timeout_ms: Option<u64>,
) -> Result<(i64, Box<dyn std::io::Read>), String> {
    let mut builder =
        ureq::AgentBuilder::new().timeout_connect(std::time::Duration::from_secs(30));
    if let Some(ms) = timeout_ms {
        builder = builder.timeout_read(std::time::Duration::from_millis(ms));
    }
    let agent = builder.build();
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let result = if body.is_empty() { req.call() } else { req.send_string(body) };
    match result {
        Ok(resp) | Err(ureq::Error::Status(_, resp)) => {
            let status = resp.status() as i64;
            Ok((status, resp.into_reader()))
        }
        Err(e) => Err(format!("HTTP stream {method} to {url} failed: {e}")),
    }
}

/// Split a `ureq::Response` into `(status, body, response_headers)`. Headers are collected
/// before the body, since `into_string` consumes the response.
#[cfg(feature = "http")]
fn collect_response(resp: ureq::Response, max_body: usize) -> Result<Fetched, String> {
    let status = resp.status() as i64;
    let mut hs = Vec::new();
    // `headers_names()` yields a name once per OCCURRENCE, and `header(name)` answers
    // the FIRST match — so a repeated header (`Set-Cookie`, canonically) used to come
    // back as its first value repeated: not merely lost, wrong. `all(name)` gives
    // every value in order; visit each unique name once and take them all.
    let mut seen: Vec<String> = Vec::new();
    for name in resp.headers_names() {
        if seen.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            continue;
        }
        for v in resp.all(&name) {
            hs.push((name.clone(), v.to_string()));
            if hs.len() > MAX_RESPONSE_HEADERS {
                return Err(format!(
                    "the response carries more than {MAX_RESPONSE_HEADERS} headers"
                ));
            }
        }
        seen.push(name);
    }
    // This used to be `into_string().unwrap_or_default()`: a body-read failure — too
    // big, invalid UTF-8, a mid-body disconnect — became an EMPTY body under a intact
    // status, complete-looking and wrong. A body that could not be read is an error.
    let body = read_body(resp, max_body)?;
    Ok((status, body, hs))
}
