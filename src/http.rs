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

/// An agent with Helix's fixed policy: the given connect and per-read timeouts, `redirects`
/// followed by ureq itself (0 for every client here except `open_stream`, which has always let
/// ureq follow them), every status returned as a response rather than an error, any method
/// allowed (`QUERY`), and NO proxy. ureq 3 reads `HTTPS_PROXY`/`HTTP_PROXY` by default; ureq 2
/// never did, so an environment variable could otherwise start routing a program's requests,
/// credentials included, through a host it never named.
#[cfg(feature = "http")]
fn new_agent(connect: std::time::Duration, per_read: Option<std::time::Duration>, redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(connect))
        .timeout_per_read(per_read)
        .max_redirects(redirects)
        .max_redirects_will_error(true)
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .proxy(None)
        .build()
        .into()
}

/// The agent for a request with these limits: the shared pool unless the request
/// tunes socket behaviour, which gets a dedicated agent.
#[cfg(feature = "http")]
fn agent_for(limits: &Limits) -> ureq::Agent {
    if limits.connect_ms.is_none() && limits.read_ms.is_none() {
        return agent();
    }
    new_agent(
        std::time::Duration::from_millis(limits.connect_ms.unwrap_or(30_000)),
        Some(std::time::Duration::from_millis(limits.read_ms.unwrap_or(120_000))),
        0,
    )
}

/// One request: `method` to `url` with `headers`, and `body` when it is not empty (an empty
/// body sends none, as before). `total_ms` is a deadline for the whole exchange, body
/// included. Built as an `http::Request` because ureq 3's builder only offers the standard
/// methods by name.
#[cfg(feature = "http")]
fn send(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    total_ms: Option<u64>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut b = ureq::http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let req = b
        .body(body.as_bytes().to_vec())
        .map_err(|e| format!("HTTP {method} to {url}: the request is malformed: {e}"))?;
    let req = match total_ms {
        Some(ms) => agent
            .configure_request(req)
            .timeout_global(Some(std::time::Duration::from_millis(ms)))
            .build(),
        None => req,
    };
    agent.run(req).map_err(|e| match e {
        ureq::Error::Timeout(ureq::Timeout::Global) if total_ms.is_some() => format!(
            "HTTP {method} to {url} exceeded total_ms ({} ms): {e}",
            total_ms.unwrap_or(0)
        ),
        e => format!("HTTP {method} to {url} failed: {e}"),
    })
}

/// Read a response body to `max_body` bytes. One byte more is an ERROR that names the
/// limit and the field that raises it — the body is attacker-sized, and handing back a
/// prefix as if it were the whole is the silent-wrong-answer class.
#[cfg(feature = "http")]
fn read_body(resp: ureq::http::Response<ureq::Body>, max_body: usize) -> Result<String, String> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    resp.into_body()
        .into_reader()
        .take(max_body as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("reading the response body failed: {}", body_error(&e)))?;
    if buf.len() > max_body {
        return Err(format!(
            "the response body exceeds max_body ({max_body} bytes) — raise `max_body` in the request record, or stream it with `http_stream`"
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| "the response body is not valid UTF-8 text".to_string())
}

/// A body-read failure in words that name the limit that fired, when it was one of ours.
#[cfg(feature = "http")]
fn body_error(e: &std::io::Error) -> String {
    match e.get_ref().and_then(|inner| inner.downcast_ref::<ureq::Error>()) {
        Some(ureq::Error::Timeout(ureq::Timeout::Global)) => {
            "timed out: the request's total deadline (`total_ms`) passed".to_string()
        }
        Some(ureq::Error::Timeout(ureq::Timeout::PerRead)) => {
            "timed out: no data arrived within the read timeout (`read_ms`, default 120 s)".to_string()
        }
        _ => e.to_string(),
    }
}

#[cfg(feature = "http")]
fn agent() -> ureq::Agent {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            // Connect/read timeouts so a hung or slow-loris server cannot stall the
            // program indefinitely; `read_body` caps the body.
            // We follow redirects ourselves (see `follow`), applying the boundary rules
            // ureq cannot express. ureq must never follow one we have not vetted, so it
            // returns every 3xx to us (max_redirects 0).
            new_agent(std::time::Duration::from_secs(30), Some(std::time::Duration::from_secs(120)), 0)
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
        // The jar's cookies for THIS hop's URL — recomputed per hop, so a redirect to a
        // new host sends that host's cookies, not the previous one's (the cross-origin
        // header strip above removed the old Cookie header; this adds the right one).
        let now = crate::cookiejar::now_unix();
        let mut hop_headers = cur_headers.clone();
        if let Some(j) = jar
            && let Some(cookie) = j.cookie_header(&cur_url, now)
        {
            hop_headers.push(("Cookie".to_string(), cookie));
        }
        // Every status comes back as a response (http_status_as_error is off), 3xx
        // included; a genuine transport failure is the only `Err`.
        let resp = send(&agent_for(limits), &cur_method, cur_url.as_str(), &hop_headers, &cur_body, limits.total_ms)?;
        let code = resp.status().as_u16() as i64;
        // A 3xx with a `Location` is a redirect to vet and follow; anything else is the
        // final response, returned as data.
        if (300..400).contains(&code)
            && let Some(location) = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
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
                    for v in resp.headers().get_all("set-cookie") {
                        if let Ok(v) = v.to_str() {
                            j.store_from_header(host, v, now);
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
    // ureq follows up to 5 redirects here itself, as it always has for streams.
    let agent = new_agent(
        std::time::Duration::from_secs(30),
        timeout_ms.map(std::time::Duration::from_millis),
        5,
    );
    let resp = send(&agent, method, url, headers, body, None)
        .map_err(|e| e.replacen("HTTP ", "HTTP stream ", 1))?;
    let status = resp.status().as_u16() as i64;
    Ok((status, Box::new(resp.into_body().into_reader())))
}

/// Split a response into `(status, body, response_headers)`. Headers are collected before
/// the body, since reading the body consumes the response.
#[cfg(feature = "http")]
fn collect_response(resp: ureq::http::Response<ureq::Body>, max_body: usize) -> Result<Fetched, String> {
    let status = resp.status().as_u16() as i64;
    let mut hs = Vec::new();
    // Each unique name once, in first-arrival order, with EVERY value it carries in
    // order: a repeated header (`Set-Cookie`, canonically) must not collapse to its
    // first value. Names are lowercase, as they have always been here. A value that is
    // not text (obs-text bytes) is kept, read as Latin-1, rather than dropped.
    for name in resp.headers().keys() {
        for v in resp.headers().get_all(name) {
            let text = match v.to_str() {
                Ok(t) => t.to_string(),
                Err(_) => v.as_bytes().iter().map(|&b| b as char).collect(),
            };
            hs.push((name.as_str().to_string(), text));
            if hs.len() > MAX_RESPONSE_HEADERS {
                return Err(format!(
                    "the response carries more than {MAX_RESPONSE_HEADERS} headers"
                ));
            }
        }
    }
    // This used to be `into_string().unwrap_or_default()`: a body-read failure — too
    // big, invalid UTF-8, a mid-body disconnect — became an EMPTY body under a intact
    // status, complete-looking and wrong. A body that could not be read is an error.
    let body = read_body(resp, max_body)?;
    Ok((status, body, hs))
}
