//! A minimal HTTP client for fetching data from a Helix program — the common
//! scientific "pull data from a URL / REST API" need. `http_get(url)` returns a
//! record `{status, body}`; the body is typically fed to `parse_json`. Behind the
//! default-on `http` feature (build `--no-default-features` for a network-free
//! binary). HTTPS is preferred; an HTTP error status is returned as data (so a 404
//! is a `status`, not a crash), while transport failures are Helix errors.

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
