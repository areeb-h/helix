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
                .build()
        })
        .clone()
}

#[cfg(feature = "http")]
pub fn get(url: &str) -> Result<(i64, String), String> {
    match agent().get(url).call() {
        Ok(resp) => {
            let status = resp.status() as i64;
            let body = resp
                .into_string()
                .map_err(|e| format!("reading the response body failed: {e}"))?;
            Ok((status, body))
        }
        // A non-2xx status isn't an error here — return the code + body so callers
        // can branch on it (e.g. handle 404 themselves).
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((code as i64, body))
        }
        Err(e) => Err(format!("HTTP request to {url} failed: {e}")),
    }
}

/// `http_post(url, body)` — send `body` with the given `content_type` and return
/// `{status, body}`, mirroring [`get`]: a non-2xx status is data (so a REST 400/422 is a
/// `status`, not a crash), transport failures are errors. `body` is sent verbatim — the
/// caller builds it (typically `record.to_json()` for a JSON API, the default content type).
#[cfg(feature = "http")]
pub fn post(url: &str, body: &str, content_type: &str) -> Result<(i64, String), String> {
    match agent().post(url).set("Content-Type", content_type).send_string(body) {
        Ok(resp) => {
            let status = resp.status() as i64;
            let rbody = resp
                .into_string()
                .map_err(|e| format!("reading the response body failed: {e}"))?;
            Ok((status, rbody))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let rbody = resp.into_string().unwrap_or_default();
            Ok((code as i64, rbody))
        }
        Err(e) => Err(format!("HTTP POST to {url} failed: {e}")),
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
) -> Result<Fetched, String> {
    let mut req = agent().request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    // An empty body → `call()` (no request body); otherwise send it as-is.
    let result = if body.is_empty() { req.call() } else { req.send_string(body) };
    match result {
        Ok(resp) => Ok(collect_response(resp)),
        // A non-2xx status still carries a full response — return it as data, not an error.
        Err(ureq::Error::Status(_, resp)) => Ok(collect_response(resp)),
        Err(e) => Err(format!("HTTP {method} to {url} failed: {e}")),
    }
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
fn collect_response(resp: ureq::Response) -> Fetched {
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
        }
        seen.push(name);
    }
    let body = resp.into_string().unwrap_or_default();
    (status, body, hs)
}
