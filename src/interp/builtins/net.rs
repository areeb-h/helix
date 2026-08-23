//! Builtins: HTTP client/server entry points and cookie parsing — moved verbatim from the one-file dispatch
//! (2026-08-24). The `call` guard names exactly the arms this file holds;
//! `dispatch` is the original match text, arm for arm.

use std::rc::Rc;

use crate::error::HelixError;
use crate::value::Value;

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;

#[inline]
pub(super) fn a_listen(args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        if args.is_empty() || args.len() > 2 {
            return Err(HelixError::new(
                format!("`listen` takes a port and an optional shard count, got {}", args.len()),
                line,
                col,
            ));
        }
        let port = match &args[0] {
            Value::Int(n) => *n,
            other => return Err(type_err("listen", "a port number", other, line, col)),
        };
        let shards = match args.get(1) {
            None => 1,
            Some(Value::Int(n)) => *n,
            Some(other) => return Err(type_err("listen", "a shard count", other, line, col)),
        };
        crate::serve::listen(port, shards, line, col)
    
}

#[inline]
pub(super) fn a_http_get(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Str(url) => {
                #[cfg(feature = "http")]
                {
                    let (status, body) =
                        crate::http::get(url).map_err(|e| HelixError::new(e, line, col))?;
                    // `{status, body}` — body is usually fed to `parse_json`.
                    Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("status"), Value::Int(status)),
                        (Symbol::intern("body"), Value::Str(Rc::new(body))),
                    ])))
                }
                #[cfg(not(feature = "http"))]
                {
                    let _ = url;
                    Err(HelixError::new(
                        "this build has no HTTP support",
                        line,
                        col,
                    )
                    .hint("build without `--no-default-features`, or with `--features http`."))
                }
            }
            other => Err(type_err("http_get", "a URL string", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_http_post(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // `http_post(url, body)` → `{status, body}`, mirroring `http_get`. The body
        // is sent verbatim with `Content-Type: application/json` (the dominant REST
        // case — the caller usually passes `record.to_json()`); custom methods and
        // headers will arrive via a general `http_request` primitive.
        arity(name, &args, 2, line, col)?;
        match (&args[0], &args[1]) {
            (Value::Str(url), Value::Str(body)) => {
                #[cfg(feature = "http")]
                {
                    let (status, resp) = crate::http::post(url, body, "application/json")
                        .map_err(|e| HelixError::new(e, line, col))?;
                    Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("status"), Value::Int(status)),
                        (Symbol::intern("body"), Value::Str(Rc::new(resp))),
                    ])))
                }
                #[cfg(not(feature = "http"))]
                {
                    let _ = (url, body);
                    Err(HelixError::new("this build has no HTTP support", line, col)
                        .hint("build without `--no-default-features`, or with `--features http`."))
                }
            }
            (Value::Str(_), other) => {
                Err(type_err("http_post", "a string body", other, line, col))
            }
            (other, _) => Err(type_err("http_post", "a URL string", other, line, col)),
        }
    
}

#[inline]
pub(super) fn a_http_request(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // The general client: `http_request({method, url, body?, headers?})` →
        // `{status, body, headers}`. One primitive for PUT/DELETE/PATCH + custom
        // request headers + returned response headers; get/post are the shortcuts.
        arity(name, &args, 1, line, col)?;
        validate_request_fields(
            &args[0],
            &[
                "method", "url", "body", "headers", "jar", "total_ms", "connect_ms",
                "read_ms", "max_body",
            ],
            name,
            line,
            col,
        )?;
        let (method, url, body, hdrs) = http_request_fields(&args[0], line, col)?;
        #[cfg(feature = "http")]
        {
            let limits = http_limits(&args[0], line, col)?;
            // An optional `jar:` field carries a cookie jar (from `cookie_jar()`);
            // the request sends its matching cookies and stores what the response
            // sets. The jar mutates through its RefCell — the program holds it.
            let jar_handle = http_jar(&args[0], line, col)?;
            let jar_ref = jar_handle.as_deref().and_then(|h| match h {
                crate::serve::NetHandle::CookieJar(j) => Some(j),
                _ => None,
            });
            let (status, rbody, rhdrs, redirects) =
                crate::http::request(&method, &url, &body, &hdrs, &limits, jar_ref)
                    .map_err(|e| HelixError::new(e, line, col))?;
            // A Headers value, not a Dict: lookup is case-insensitive (one
            // program sees `Content-Type` from HTTP/1.1 and `content-type`
            // from HTTP/2), wire order is kept, and a repeated name —
            // `Set-Cookie` legitimately repeats — survives where a map
            // collapsed it silently.
            Ok(Value::Record(Rc::new(vec![
                (Symbol::intern("status"), Value::Int(status)),
                (Symbol::intern("body"), Value::Str(Rc::new(rbody))),
                (Symbol::intern("headers"), Value::Headers(Rc::new(rhdrs))),
                // The chain of URLs redirected through — empty when direct.
                // Data, not a silent follow (ADR 0031).
                (
                    Symbol::intern("redirects"),
                    Value::array(redirects.into_iter().map(|u| Value::Str(Rc::new(u))).collect()),
                ),
            ])))
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (&method, &url, &body, &hdrs);
            Err(HelixError::new("this build has no HTTP support", line, col)
                .hint("build without `--no-default-features`, or with `--features http`."))
        }
    
}

#[inline]
pub(super) fn a_http_stream(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // Streaming client: `http_stream({method, url, body?, headers?})` opens the
        // request and returns a handle the program pulls line-by-line — `s.status()`
        // then `s.next()` in a loop, `missing` at EOF — for token-by-token model
        // output (Ollama NDJSON / OpenAI SSE), the client mirror of accept→send.
        arity(name, &args, 1, line, col)?;
        validate_request_fields(
            &args[0],
            &["method", "url", "body", "headers", "timeout_ms"],
            name,
            line,
            col,
        )?;
        let (method, url, body, hdrs) = http_request_fields(&args[0], line, col)?;
        // Optional `timeout_ms` (per-chunk read deadline) — a positive integer field.
        let timeout_ms = http_timeout_ms(&args[0], line, col)?;
        #[cfg(feature = "http")]
        {
            let (status, reader) =
                crate::http::open_stream(&method, &url, &body, &hdrs, timeout_ms)
                    .map_err(|e| HelixError::new(e, line, col))?;
            Ok(Value::Net(Rc::new(crate::serve::NetHandle::HttpStream {
                status,
                reader: std::cell::RefCell::new(Some(std::io::BufReader::new(reader))),
            })))
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = (&method, &url, &body, &hdrs, &timeout_ms);
            Err(HelixError::new("this build has no HTTP support", line, col)
                .hint("build without `--no-default-features`, or with `--features http`."))
        }
    
}

#[inline]
pub(super) fn a_cookie_jar(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 0, line, col)?;
        Ok(Value::Net(Rc::new(crate::serve::NetHandle::CookieJar(
            crate::cookiejar::CookieJar::new(),
        ))))
    
}

// A `Cookie:` request header — `name=value; name2=value2` — as a Dict.
// Pairs are separated by `;` (never `,`), a value may be quoted, and
// surrounding spaces are not part of either half.

#[inline]
pub(super) fn a_parse_cookies(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Missing => Ok(Value::Missing),
            Value::Str(s) => {
                let mut map = std::collections::BTreeMap::new();
                for part in s.split(';') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    // A cookie with no `=` is not a pair; skip it rather than
                    // invent a name for it.
                    if let Some((k, v)) = part.split_once('=') {
                        let k = k.trim();
                        if k.is_empty() {
                            continue;
                        }
                        let v = v.trim().trim_matches('"');
                        map.insert(
                            crate::value::DictKey::Str(Rc::new(k.to_string())),
                            Value::Str(Rc::new(v.to_string())),
                        );
                    }
                }
                Ok(Value::Dict(Rc::new(map)))
            }
            other => Err(type_err("parse_cookies", "a string", other, line, col)),
        }
    
}

// One `Set-Cookie:` response header as a record: the pair, plus the
// attributes that decide whether it is stored and sent back.
//
// Attribute names are matched case-insensitively (RFC 6265 says they are),
// `Secure` and `HttpOnly` are flags with no value, and everything not
// recognised is ignored rather than guessed at. `max_age` is an Int because
// it is a number of seconds; `expires` stays the raw string, since parsing
// an HTTP-date is a bigger question than this and the raw value is what a
// caller forwards anyway.

#[inline]
pub(super) fn a_parse_set_cookie(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        arity(name, &args, 1, line, col)?;
        match &args[0] {
            Value::Missing => Ok(Value::Missing),
            Value::Str(s) => {
                // ONE parser, shared with the cookie jar (`store_from_header`),
                // so the record a program reads and the cookie the jar stores can
                // never disagree about a header.
                let Some(c) = crate::cookiejar::parse_set_cookie(s) else {
                    return Err(HelixError::new(
                        "`parse_set_cookie` needs a `name=value` pair before the first `;`",
                        line,
                        col,
                    )
                    .hint("e.g. `parse_set_cookie(\"id=abc; Path=/; Secure\")`."));
                };
                let mut fields: Vec<(Symbol, Value)> = vec![
                    (Symbol::intern("name"), Value::Str(Rc::new(c.name))),
                    (Symbol::intern("value"), Value::Str(Rc::new(c.value))),
                ];
                if let Some(p) = c.path {
                    fields.push((Symbol::intern("path"), Value::Str(Rc::new(p))));
                }
                if let Some(d) = c.domain {
                    fields.push((Symbol::intern("domain"), Value::Str(Rc::new(d))));
                }
                // Expires kept verbatim — an HTTP-date carries a comma, which is
                // why `Set-Cookie` is not comma-combinable; the jar parses it, a
                // reader usually just forwards it.
                if let Some(e) = c.expires {
                    fields.push((Symbol::intern("expires"), Value::Str(Rc::new(e))));
                }
                if let Some(ss) = c.same_site {
                    fields.push((Symbol::intern("same_site"), Value::Str(Rc::new(ss))));
                }
                if let Some(ma) = c.max_age {
                    fields.push((Symbol::intern("max_age"), Value::Int(ma)));
                }
                fields.push((Symbol::intern("secure"), Value::Bool(c.secure)));
                fields.push((Symbol::intern("http_only"), Value::Bool(c.http_only)));
                Ok(Value::Record(Rc::new(fields)))
            }
            other => Err(type_err("parse_set_cookie", "a string", other, line, col)),
        }
    
}

// Percent-encoding, RFC 3986. Encodes BYTES, not characters: `é` is two
// UTF-8 bytes and becomes `%C3%A9`, which a per-character mapping cannot
// express. Only the unreserved set survives; uppercase hex, as the RFC
// says producers should emit.

#[inline]
pub(super) fn a_headers(name: &str, args: Vec<Value>, line: usize, col: usize) -> Result<Value, HelixError> {
        // Construct a `Headers` value from wire-ordered (name, value) pairs —
        // the same type a live response carries, so a test double can BE the
        // real thing (the field's mocks were case-sensitive Dicts that
        // diverged from live traffic at exactly the boundary tests cross).
        // Order kept, repeats kept; names and values validated like every
        // other header that reaches the wire.
        arity(name, &args, 1, line, col)?;
        let items = match &args[0] {
            Value::Array(a) => a,
            other => {
                return Err(type_err(
                    "headers",
                    "an array of (name, value) pairs",
                    other,
                    line,
                    col,
                )
                .hint("e.g. `headers([(\"Content-Type\", \"text/html\")])`."))
            }
        };
        let mut out: Vec<(String, String)> = Vec::with_capacity(items.len());
        for (i, item) in items.iter_values().enumerate() {
            let pair: Option<(Value, Value)> = match &item {
                Value::Tuple(t) if t.len() == 2 => Some((t[0].clone(), t[1].clone())),
                Value::Array(a) if a.len() == 2 => Some((a.get(0), a.get(1))),
                _ => None,
            };
            let Some((k, v)) = pair else {
                return Err(HelixError::new(
                    format!(
                        "`headers` needs (name, value) pairs, but element {i} is {}",
                        crate::value::with_article(item.type_name())
                    ),
                    line,
                    col,
                )
                .hint("each element must hold exactly two strings."));
            };
            let (Value::Str(k), Value::Str(v)) = (&k, &v) else {
                return Err(HelixError::new(
                    format!(
                        "`headers` needs string names and values, but pair {i} holds {} and {}",
                        crate::value::with_article(k.type_name()),
                        crate::value::with_article(v.type_name())
                    ),
                    line,
                    col,
                ));
            };
            crate::value::validate_header(k, v).map_err(|m| HelixError::new(m, line, col))?;
            out.push(((**k).clone(), (**v).clone()));
        }
        Ok(Value::Headers(Rc::new(out)))
    
}
