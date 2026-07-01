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

#[cfg(feature = "http")]
pub fn get(url: &str) -> Result<(i64, String), String> {
    // Connect/read timeouts so a hung or slow-loris server can't stall the program
    // indefinitely. (`into_string` already caps the body at ureq's 10 MiB limit.)
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    match agent.get(url).call() {
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
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    match agent.post(url).set("Content-Type", content_type).send_string(body) {
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
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    let mut req = agent.request(method, url);
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
/// `http_stream` for token-by-token model output. No read timeout — a streaming response may
/// idle between chunks (a model thinking) far longer than a normal body read, and the program
/// decides when to stop.
#[cfg(feature = "http")]
pub fn open_stream(
    method: &str,
    url: &str,
    body: &str,
    headers: &[(String, String)],
) -> Result<(i64, Box<dyn std::io::Read>), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .build();
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
    for name in resp.headers_names() {
        if let Some(v) = resp.header(&name) {
            hs.push((name, v.to_string()));
        }
    }
    let body = resp.into_string().unwrap_or_default();
    (status, body, hs)
}
