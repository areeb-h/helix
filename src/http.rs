//! A minimal HTTP client for fetching data from a Helix program — the common
//! scientific "pull data from a URL / REST API" need. `http_get(url)` returns a
//! record `{status, body}`; the body is typically fed to `parse_json`. Behind the
//! default-on `http` feature (build `--no-default-features` for a network-free
//! binary). HTTPS is preferred; an HTTP error status is returned as data (so a 404
//! is a `status`, not a crash), while transport failures are Helix errors.

#[cfg(feature = "http")]
pub fn get(url: &str) -> Result<(i64, String), String> {
    match ureq::get(url).call() {
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
