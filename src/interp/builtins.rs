//! The built-in function dispatch (`print`, math, `range`, `dna`, `tensor`,
//! `read_csv`/`read_parquet`/`read_fasta`, `write_parquet`, …) — an `impl Interp`
//! method, split out from the core evaluator. The numeric/shape helper free
//! functions it uses (`broadcast_unary`, `apply_float_fn`, `int_range`, …) stay in
//! the parent module and are reached via `use super::*`.

use super::*;

/// How many assertions this process has executed, counted at the one dispatch point the
/// tree-walker and the VM share. `helix test` reads it to fail a test file that ran to
/// completion without asserting anything — a file whose checks all sit inside `test_*`
/// functions nobody calls used to report `ok`, which is the worst possible answer from a
/// test runner. Not reset by `helix run`; `cli_test` resets it per file.
pub static ASSERTIONS_RUN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl super::Interp {
    pub(crate) fn call_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        // Capability gate (ADR 0021): authority-bearing builtins (fs/net) consult the
        // process authority first. A no-op under the default `Off` mode and for `pure`
        // builtins; logs (audit) or denies (enforce) an ungranted access otherwise.
        crate::capability::gate(name, &args, line, col)?;
        // Counted before the arm runs, so a FAILING assertion counts too: the file fails
        // on the raise, and "asserted nothing" must not also be reported about it.
        if matches!(name, "assert" | "assert_eq" | "assert_close") {
            ASSERTIONS_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        match name {
            "print" => {
                // Rich rendering on a terminal (tables, color, elision, grouped
                // numbers); byte-identical to the plain `display_value` join when
                // piped/redirected. A DataFrame argument still materializes here, so
                // a failed query is a real error (non-zero exit), never a swallowed
                // placeholder printed as if the program succeeded.
                println!("{}", crate::render::render_print(&args, line, col)?);
                Ok(Value::Unit)
            }
            // `emit(x)` — write one value as a single PLAIN line and FLUSH immediately, so a
            // downstream consumer sees it now rather than at exit. `print` is block-buffered
            // when piped (and rich-formats for a terminal); `emit` is the streaming sink —
            // one value per line, machine-readable (pair with `x.to_json()` for NDJSON).
            "emit" => {
                arity(name, &args, 1, line, col)?;
                use std::io::Write;
                let s = crate::value::display_value(&args[0], line, col)?;
                let mut out = std::io::stdout().lock();
                // Errors writing to a closed pipe are the consumer's business, not a Helix
                // runtime error (a piped reader exiting first is normal); ignore them.
                let _ = writeln!(out, "{s}");
                let _ = out.flush();
                Ok(Value::Unit)
            }
            // `write(x)` — the inline sibling of `emit`: same plain, flush-now streaming sink,
            // but with NO trailing newline, so tokens flow across one line like a live chat UI
            // (`for t in stream: write(t)`). `emit` frames one value per line; `write` frames
            // nothing — the program decides where breaks go (a final `emit("")` ends the line).
            "write" => {
                arity(name, &args, 1, line, col)?;
                use std::io::Write;
                let s = crate::value::display_value(&args[0], line, col)?;
                let mut out = std::io::stdout().lock();
                let _ = write!(out, "{s}");
                let _ = out.flush();
                Ok(Value::Unit)
            }
            // `elog(x)` — `emit` for the **stderr** channel: one value per line, flushed now.
            // Lets a program stream results on stdout while sending progress/logs to stderr, so
            // the two don't interleave when stdout is piped (`helix run x.helix | consumer`).
            "elog" => {
                arity(name, &args, 1, line, col)?;
                use std::io::Write;
                let s = crate::value::display_value(&args[0], line, col)?;
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "{s}");
                let _ = err.flush();
                Ok(Value::Unit)
            }
            // `read_int()` — read one line from stdin and parse it as an integer. The
            // console-input primitive (the companion to `print`/`emit`). Returns
            // `missing` on end-of-input or a non-numeric line, so a program can detect
            // "no more input" without crashing (ADR 0001). Non-deterministic, so it is
            // outside the differential oracle.
            "read_int" => {
                arity(name, &args, 0, line, col)?;
                use std::io::BufRead;
                let mut buf = String::new();
                match std::io::stdin().lock().read_line(&mut buf) {
                    Ok(0) => Ok(Value::Missing), // EOF
                    Ok(_) => Ok(buf.trim().parse::<i64>().map(Value::Int).unwrap_or(Value::Missing)),
                    Err(_) => Ok(Value::Missing),
                }
            }
            // `sleep(ms)` — pause the program for `ms` milliseconds (wall clock). The
            // pacing primitive for a paced loop (`emit(frame)` then `sleep(16)` ≈ 60 fps);
            // pairs with `clock_monotonic()`. A non-deterministic effect, so (like `print`/
            // `emit`) it lives outside the differential oracle. Fractional ms are honoured.
            "sleep" => {
                arity(name, &args, 1, line, col)?;
                let ms = match &args[0] {
                    Value::Int(n) => *n as f64,
                    Value::Float(f) => *f,
                    other => {
                        return Err(type_err("sleep", "a number of milliseconds", other, line, col))
                    }
                };
                if !ms.is_finite() || ms < 0.0 {
                    return Err(HelixError::new(
                        "`sleep` needs a non-negative, finite number of milliseconds",
                        line,
                        col,
                    ));
                }
                std::thread::sleep(std::time::Duration::from_secs_f64(ms / 1000.0));
                Ok(Value::Unit)
            }
            "listen" => {
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
            "assert" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`assert` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                match &args[0] {
                    Value::Bool(true) => Ok(Value::Unit),
                    Value::Bool(false) | Value::Missing => {
                        // A `missing` condition is not provably true → a failed assertion.
                        let msg = match args.get(1) {
                            Some(Value::Str(s)) => format!("assertion failed: {s}"),
                            Some(other) => format!(
                                "assertion failed: {}",
                                crate::value::display_value(other, line, col)?
                            ),
                            None => "assertion failed".to_string(),
                        };
                        Err(HelixError::new(msg, line, col))
                    }
                    other => Err(type_err("assert", "a boolean condition", other, line, col)),
                }
            }
            // `raise(message[, help])` — a library reporting its CALLER's mistake. The only
            // mechanism before this was `assert`, which hard-codes "assertion failed: " and
            // cannot carry a `help:` line, so `route.go("admin")` came back as
            // "assertion failed: route path must start with '/'" — which reads as a broken
            // library rather than a rejected argument. ADR 0004 leaves open whether user
            // errors can be as instructive as the interpreter's own; they can now.
            //
            // Caught by `try` like any other error, because it IS one — the same
            // `HelixError` the runtime raises everywhere else, with no special variant.
            "raise" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`raise` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let text = |v: &Value| match v {
                    Value::Str(s) => Ok(s.to_string()),
                    other => crate::value::display_value(other, line, col),
                };
                let e = HelixError::new(text(&args[0])?, line, col);
                Err(match args.get(1) {
                    Some(h) => e.hint(text(h)?),
                    None => e,
                })
            }
            // `source_path(rel)` — `rel` against the directory of the file this call is
            // WRITTEN in, so a package can read the data it ships no matter where the
            // process was started. Every path used to resolve against the CWD, which meant
            // a library could not ship a scoring matrix, a codon table or a reference panel
            // at all: running the same program from a subdirectory broke it.
            //
            // Already-absolute input is returned untouched, so wrapping a user-supplied
            // path is harmless.
            "source_path" => {
                arity(name, &args, 1, line, col)?;
                let Value::Str(rel) = &args[0] else {
                    return Err(type_err("source_path", "a string path", &args[0], line, col));
                };
                let p = std::path::Path::new(rel.as_str());
                if p.is_absolute() {
                    return Ok(Value::Str(rel.clone()));
                }
                // `line` is the GLOBAL line in the flattened program, which is exactly what
                // identifies the module it came from.
                let dir = crate::module::file_of_line(line)
                    .and_then(|f| std::path::Path::new(&f).parent().map(|d| d.to_path_buf()));
                let Some(dir) = dir else {
                    return Err(HelixError::new(
                        "`source_path` needs a source file to resolve against",
                        line,
                        col,
                    )
                    .hint(
                        "it answers \"where is the file I am written in?\", so it has no \
                         meaning in the REPL — pass an absolute path instead.",
                    ));
                };
                Ok(Value::Str(dir.join(p).to_string_lossy().into_owned().into()))
            }
            "assert_eq" => {
                arity(name, &args, 2, line, col)?;
                if values_equal(&args[0], &args[1]) {
                    Ok(Value::Unit)
                } else {
                    let a = crate::value::display_value(&args[0], line, col)?;
                    let b = crate::value::display_value(&args[1], line, col)?;
                    Err(HelixError::new(format!("assertion failed: {a} != {b}"), line, col))
                }
            }
            "assert_close" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`assert_close` takes 2 or 3 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let num = |v: &Value| -> Result<f64, HelixError> {
                    match v {
                        Value::Int(i) => Ok(*i as f64),
                        Value::Float(f) => Ok(*f),
                        other => Err(type_err("assert_close", "a number", other, line, col)),
                    }
                };
                let (a, b) = (num(&args[0])?, num(&args[1])?);
                // Default tolerance suits the f64 round-off of typical scientific compute.
                let tol = match args.get(2) {
                    Some(v) => num(v)?,
                    None => 1e-9,
                };
                if (a - b).abs() <= tol {
                    Ok(Value::Unit)
                } else {
                    Err(HelixError::new(
                        format!("assertion failed: {a} is not within {tol} of {b}"),
                        line,
                        col,
                    ))
                }
            }
            "dna" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => make_dna(s, line, col),
                    other => Err(type_err("dna", "a string", other, line, col)),
                }
            }
            "to_array" => {
                arity(name, &args, 1, line, col)?;
                match args.into_iter().next().unwrap() {
                    // A Tensor flattens NATIVELY (row-major) — this was Python-gated,
                    // which put a feature wall exactly between the two halves of a
                    // network: the BLAS tensor path computes at ~177 GFLOPS and the
                    // autodiff tape consumes `[Float]`, and on a stock binary nothing
                    // could cross (the nn field report's top finding). A tracked
                    // tensor's payload crosses the same way, via its VALUE — the tape
                    // is not extended, exactly like `value_of`.
                    Value::Tensor(t) => Ok(Value::float_array(t.iter().copied().collect())),
                    Value::Node(n) => match crate::autodiff::node_value(&n) {
                        Value::Tensor(t) => {
                            Ok(Value::float_array(t.iter().copied().collect()))
                        }
                        other => Ok(Value::array(vec![other])),
                    },
                    // Explicit, on-demand materialization of a Python iterable into a
                    // native Helix Array (the visible escape hatch from
                    // opaque-by-default).
                    other => crate::python::to_array(other, line, col),
                }
            }
            "to_dataframe" => {
                arity(name, &args, 1, line, col)?;
                // Bring a Python polars/pandas/pyarrow frame into Helix as a native
                // DataFrame, zero-copy via Arrow.
                crate::python::to_dataframe(args.into_iter().next().unwrap(), line, col)
            }
            "to_tensor" => {
                arity(name, &args, 1, line, col)?;
                // Bring a Python NumPy f64 array into Helix as a native Tensor.
                crate::python::to_tensor(args.into_iter().next().unwrap(), line, col)
            }
            // JSON, charts, writers, and format export are now methods (see
            // `interp::methods` / `interp::export_method`): `str.parse_json()`,
            // `value.to_json()`, `xs.bar_chart()`, `data.to_html()`, `df.write_csv(p)`.
            // Reproducible RNG — seeded + pure (same seed → same draws).
            "random" => crate::rng::random(&args, line, col),
            "randn" => crate::rng::randn(&args, line, col),
            "random_int" => crate::rng::random_int(&args, line, col),
            "http_get" => {
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
            "http_post" => {
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
            "http_request" => {
                // The general client: `http_request({method, url, body?, headers?})` →
                // `{status, body, headers}`. One primitive for PUT/DELETE/PATCH + custom
                // request headers + returned response headers; get/post are the shortcuts.
                arity(name, &args, 1, line, col)?;
                let (method, url, body, hdrs) = http_request_fields(&args[0], line, col)?;
                #[cfg(feature = "http")]
                {
                    let (status, rbody, rhdrs) = crate::http::request(&method, &url, &body, &hdrs)
                        .map_err(|e| HelixError::new(e, line, col))?;
                    let mut hmap = std::collections::BTreeMap::new();
                    for (k, v) in rhdrs {
                        hmap.insert(crate::value::DictKey::Str(Rc::new(k)), Value::Str(Rc::new(v)));
                    }
                    Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("status"), Value::Int(status)),
                        (Symbol::intern("body"), Value::Str(Rc::new(rbody))),
                        (Symbol::intern("headers"), Value::Dict(Rc::new(hmap))),
                    ])))
                }
                #[cfg(not(feature = "http"))]
                {
                    let _ = (&method, &url, &body, &hdrs);
                    Err(HelixError::new("this build has no HTTP support", line, col)
                        .hint("build without `--no-default-features`, or with `--features http`."))
                }
            }
            "http_stream" => {
                // Streaming client: `http_stream({method, url, body?, headers?})` opens the
                // request and returns a handle the program pulls line-by-line — `s.status()`
                // then `s.next()` in a loop, `missing` at EOF — for token-by-token model
                // output (Ollama NDJSON / OpenAI SSE), the client mirror of accept→send.
                arity(name, &args, 1, line, col)?;
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
            "read_csv" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(dataframe::read_csv(s, line, col)?)),
                    other => Err(type_err("read_csv", "a string path", other, line, col)),
                }
            }
            // Generic text/JSON readers — the non-tabular counterpart to read_csv,
            // for reloading config, library definitions, and saved JSON across runs.
            "read_text" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::Str(Rc::new(read_text_file(s, line, col)?))),
                    other => Err(type_err("read_text", "a string path", other, line, col)),
                }
            }
            "read_json" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let text = read_text_file(s, line, col)?;
                        crate::json::parse(&text)
                            .map_err(|m| HelixError::new(format!("invalid JSON in `{s}`: {m}"), line, col))
                    }
                    other => Err(type_err("read_json", "a string path", other, line, col)),
                }
            }
            "file_exists" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::Bool(std::path::Path::new(s.as_str()).exists())),
                    other => Err(type_err("file_exists", "a string path", other, line, col)),
                }
            }
            // List a directory's immediate entries as full (dir-joined) path strings,
            // sorted for a reproducible order — so the result feeds straight into
            // `read_csv`/`read_text` and a program can ingest "whatever files exist".
            "read_dir" => {
                arity(name, &args, 1, line, col)?;
                let dir = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("read_dir", "a directory path string", other, line, col)),
                };
                let rd = std::fs::read_dir(dir.as_str()).map_err(|e| {
                    HelixError::new(format!("could not read directory `{dir}`: {e}"), line, col)
                        .hint("check the path exists and is a directory.")
                })?;
                let mut paths = Vec::new();
                for entry in rd {
                    let entry = entry.map_err(|e| {
                        HelixError::new(format!("error reading directory `{dir}`: {e}"), line, col)
                    })?;
                    paths.push(entry.path().to_string_lossy().into_owned());
                }
                paths.sort();
                Ok(Value::array(paths.into_iter().map(|p| Value::Str(Rc::new(p))).collect()))
            }
            // Storage-lifecycle ops. `remove_file` is idempotent (false if the file
            // wasn't there); `mkdir` makes the directory and any parents, returning
            // whether it was newly created. Both error only on a real I/O failure.
            "remove_file" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let p = std::path::Path::new(s.as_str());
                        if p.exists() {
                            std::fs::remove_file(p).map_err(|e| {
                                HelixError::new(format!("could not remove `{s}`: {e}"), line, col)
                            })?;
                            Ok(Value::Bool(true))
                        } else {
                            Ok(Value::Bool(false))
                        }
                    }
                    other => Err(type_err("remove_file", "a string path", other, line, col)),
                }
            }
            "mkdir" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        let existed = std::path::Path::new(s.as_str()).exists();
                        std::fs::create_dir_all(s.as_str()).map_err(|e| {
                            HelixError::new(format!("could not create directory `{s}`: {e}"), line, col)
                        })?;
                        Ok(Value::Bool(!existed))
                    }
                    other => Err(type_err("mkdir", "a string path", other, line, col)),
                }
            }
            // Content hash (hex SHA-256) for content-addressing / reproducibility ids.
            "sha256" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(s.as_bytes());
                        Ok(Value::Str(Rc::new(format!("{:x}", hasher.finalize()))))
                    }
                    other => Err(type_err("sha256", "a string", other, line, col)),
                }
            }
            // HMAC-SHA256(key, message) → lowercase hex. The auth workhorse (JWT, webhook
            // and API-request signing, secure tokens) — needs raw-byte HMAC that pure Helix
            // can't do (its strings are UTF-8). Key/message are hashed by their UTF-8 bytes.
            "hmac_sha256" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let key = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("hmac_sha256", "a string key", other, line, col)),
                };
                let msg = match &args[1] {
                    Value::Str(s) => s,
                    other => return Err(type_err("hmac_sha256", "a string message", other, line, col)),
                };
                use hmac::{Hmac, Mac};
                let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key.as_bytes())
                    .expect("HMAC accepts a key of any length");
                mac.update(msg.as_bytes());
                let tag = mac.finalize().into_bytes();
                let hex: String = tag.iter().map(|b| format!("{:02x}", b)).collect();
                Ok(Value::Str(Rc::new(hex)))
            }
            "base64_encode" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    Value::Str(s) => {
                        use base64::Engine;
                        let enc = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
                        Ok(Value::Str(Rc::new(enc)))
                    }
                    other => Err(type_err("base64_encode", "a string", other, line, col)),
                }
            }
            "base64_decode" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    Value::Str(s) => {
                        use base64::Engine;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(s.as_bytes())
                            .map_err(|e| {
                                HelixError::new(format!("`base64_decode` got invalid base64: {e}"), line, col)
                            })?;
                        match String::from_utf8(bytes) {
                            Ok(text) => Ok(Value::Str(Rc::new(text))),
                            Err(_) => Err(HelixError::new(
                                "`base64_decode` produced non-UTF-8 bytes — Helix strings are text, so binary payloads aren't representable",
                                line,
                                col,
                            )),
                        }
                    }
                    other => Err(type_err("base64_decode", "a string", other, line, col)),
                }
            }
            "hex_encode" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    Value::Str(s) | Value::Dna(s) => {
                        Ok(Value::Str(Rc::new(s.bytes().map(|b| format!("{b:02x}")).collect())))
                    }
                    other => Err(type_err("hex_encode", "a string", other, line, col)),
                }
            }
            "hex_decode" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    Value::Str(s) => match hex_to_bytes(s) {
                        Some(bytes) => match String::from_utf8(bytes) {
                            Ok(t) => Ok(Value::Str(Rc::new(t))),
                            Err(_) => Err(HelixError::new(
                                "`hex_decode` produced non-UTF-8 bytes — Helix strings are text",
                                line,
                                col,
                            )),
                        },
                        None => Err(HelixError::new(
                            "`hex_decode` needs an even number of hex digits (0-9, a-f)",
                            line,
                            col,
                        )),
                    },
                    other => Err(type_err("hex_decode", "a hex string", other, line, col)),
                }
            }
            // AES-256-GCM authenticated encryption. Misuse-resistant by construction: the
            // nonce is random per call (internal, prepended to the ciphertext) so it can't be
            // reused, and decryption verifies the tag — a wrong key or tampered ciphertext is
            // an error, never a silent garbage plaintext. Keys are 64-hex (use `aes_keygen()`).
            "aes_keygen" => {
                arity(name, &args, 0, line, col)?;
                use aes_gcm::{aead::OsRng, Aes256Gcm, KeyInit};
                let key = Aes256Gcm::generate_key(&mut OsRng);
                Ok(Value::Str(Rc::new(key.iter().map(|b| format!("{b:02x}")).collect())))
            }
            "aes_encrypt" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let key_hex = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("aes_encrypt", "a 64-hex key", other, line, col)),
                };
                let plaintext = match &args[1] {
                    Value::Str(s) => s,
                    other => return Err(type_err("aes_encrypt", "a string to encrypt", other, line, col)),
                };
                let key_bytes = aes_key_bytes(key_hex, line, col)?;
                use aes_gcm::{
                    aead::{Aead, AeadCore, OsRng},
                    Aes256Gcm, Key, KeyInit,
                };
                let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
                let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // 96-bit, unique per call
                let ct = cipher
                    .encrypt(&nonce, plaintext.as_bytes())
                    .map_err(|_| HelixError::new("`aes_encrypt` failed", line, col))?;
                let mut blob = nonce.to_vec();
                blob.extend_from_slice(&ct);
                use base64::Engine;
                Ok(Value::Str(Rc::new(base64::engine::general_purpose::STANDARD.encode(&blob))))
            }
            "aes_decrypt" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let key_hex = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("aes_decrypt", "a 64-hex key", other, line, col)),
                };
                let blob_b64 = match &args[1] {
                    Value::Str(s) => s,
                    other => return Err(type_err("aes_decrypt", "a base64 ciphertext", other, line, col)),
                };
                let key_bytes = aes_key_bytes(key_hex, line, col)?;
                use base64::Engine;
                let blob = base64::engine::general_purpose::STANDARD
                    .decode(blob_b64.as_bytes())
                    .map_err(|_| HelixError::new("`aes_decrypt` got invalid base64", line, col))?;
                if blob.len() < 12 {
                    return Err(HelixError::new("`aes_decrypt` ciphertext is too short", line, col));
                }
                let (nonce, ct) = blob.split_at(12);
                use aes_gcm::{aead::Aead, Aes256Gcm, Key, KeyInit, Nonce};
                let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
                let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| {
                    HelixError::new(
                        "`aes_decrypt` failed — wrong key, or the ciphertext was tampered with",
                        line,
                        col,
                    )
                })?;
                match String::from_utf8(pt) {
                    Ok(t) => Ok(Value::Str(Rc::new(t))),
                    Err(_) => Err(HelixError::new("`aes_decrypt` produced non-UTF-8 plaintext", line, col)),
                }
            }
            // Ed25519 signatures — the safe asymmetric primitive (deterministic, no nonce
            // footgun). keygen → {private, public} (both hex); sign → 128-hex signature;
            // verify → Bool (a wrong/forged signature is `false`, never an error). Strict
            // verification (rejects malleable signatures).
            "ed25519_keygen" => {
                arity(name, &args, 0, line, col)?;
                use aes_gcm::aead::{rand_core::RngCore, OsRng};
                let mut seed = [0u8; 32];
                OsRng.fill_bytes(&mut seed);
                let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
                let pk = sk.verifying_key();
                let priv_hex: String = sk.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
                let pub_hex: String = pk.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
                Ok(Value::Record(Rc::new(vec![
                    (crate::symbol::Symbol::intern("private"), Value::Str(Rc::new(priv_hex))),
                    (crate::symbol::Symbol::intern("public"), Value::Str(Rc::new(pub_hex))),
                ])))
            }
            "ed25519_sign" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let priv_hex = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("ed25519_sign", "a 64-hex private key", other, line, col)),
                };
                let msg = match &args[1] {
                    Value::Str(s) => s,
                    other => return Err(type_err("ed25519_sign", "a string message", other, line, col)),
                };
                let seed = hex_to_array32(priv_hex, "an ed25519 private key", line, col)?;
                use ed25519_dalek::Signer;
                let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
                let sig = sk.sign(msg.as_bytes());
                Ok(Value::Str(Rc::new(sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect())))
            }
            "ed25519_verify" => {
                arity(name, &args, 3, line, col)?;
                if args.iter().any(|a| matches!(a, Value::Missing)) {
                    return Ok(Value::Missing);
                }
                let pub_hex = match &args[0] {
                    Value::Str(s) => s,
                    other => return Err(type_err("ed25519_verify", "a 64-hex public key", other, line, col)),
                };
                let msg = match &args[1] {
                    Value::Str(s) => s,
                    other => return Err(type_err("ed25519_verify", "a string message", other, line, col)),
                };
                let sig_hex = match &args[2] {
                    Value::Str(s) => s,
                    other => return Err(type_err("ed25519_verify", "a 128-hex signature", other, line, col)),
                };
                let pub_bytes = hex_to_array32(pub_hex, "an ed25519 public key", line, col)?;
                let vk = match ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes) {
                    Ok(v) => v,
                    Err(_) => return Err(HelixError::new("`ed25519_verify` got an invalid public key", line, col)),
                };
                // A malformed signature is just an invalid one → `false`, not an error.
                let sig_bytes: [u8; 64] = match hex_to_bytes(sig_hex).and_then(|b| b.try_into().ok()) {
                    Some(b) => b,
                    None => return Ok(Value::Bool(false)),
                };
                let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                // `verify_strict` rejects malleable (non-canonical) signatures.
                Ok(Value::Bool(vk.verify_strict(msg.as_bytes(), &sig).is_ok()))
            }
            "read_vcf" => {
                // `read_vcf(path)` scans; `read_vcf(path, "chr:start-end")` does an
                // indexed region query against the file's `.tbi`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`read_vcf` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let path = match &args[0] {
                    Value::Str(s) => s,
                    other => {
                        return Err(type_err("read_vcf", "a string path", other, line, col));
                    }
                };
                let df = match args.get(1) {
                    Some(Value::Str(region)) => {
                        crate::vcf::read_vcf_region(path, region, line, col)?
                    }
                    Some(other) => {
                        return Err(type_err("read_vcf", "a string region", other, line, col));
                    }
                    None => crate::vcf::read_vcf(path, line, col)?,
                };
                Ok(Value::dataframe(df))
            }
            "read_bcf" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::vcf::read_bcf(s, line, col)?)),
                    other => Err(type_err("read_bcf", "a string path", other, line, col)),
                }
            }
            "read_sam" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::sam::read_sam(s, line, col)?)),
                    other => Err(type_err("read_sam", "a string path", other, line, col)),
                }
            }
            "read_bam" => {
                // `read_bam(path)` scans; `read_bam(path, "chr:start-end")` does an
                // indexed region query against the file's `.bai`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`read_bam` takes 1 or 2 arguments, got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let path = match &args[0] {
                    Value::Str(s) => s,
                    other => {
                        return Err(type_err("read_bam", "a string path", other, line, col));
                    }
                };
                let df = match args.get(1) {
                    Some(Value::Str(region)) => {
                        crate::sam::read_bam_region(path, region, line, col)?
                    }
                    Some(other) => {
                        return Err(type_err("read_bam", "a string region", other, line, col));
                    }
                    None => crate::sam::read_bam(path, line, col)?,
                };
                Ok(Value::dataframe(df))
            }
            "read_gff" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::gff::read_gff(s, line, col)?)),
                    other => Err(type_err("read_gff", "a string path", other, line, col)),
                }
            }
            "read_bed" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(crate::bed::read_bed(s, line, col)?)),
                    other => Err(type_err("read_bed", "a string path", other, line, col)),
                }
            }
            "range" => match args.len() {
                1 => {
                    let n = as_int(&args[0], "range", line, col)?;
                    int_range(0, n, line, col)
                }
                2 => {
                    let a = as_int(&args[0], "range", line, col)?;
                    let b = as_int(&args[1], "range", line, col)?;
                    int_range(a, b, line, col)
                }
                3 => {
                    let a = as_int(&args[0], "range", line, col)?;
                    let b = as_int(&args[1], "range", line, col)?;
                    let step = as_int(&args[2], "range", line, col)?;
                    int_range_step(a, b, step, line, col)
                }
                _ => Err(HelixError::new(
                    format!("`range` takes 1 to 3 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("use `range(n)`, `range(start, stop)`, or `range(start, stop, step)`.")),
            },
            "read_parquet" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => Ok(Value::dataframe(dataframe::read_parquet(s, line, col)?)),
                    other => Err(type_err("read_parquet", "a string path", other, line, col)),
                }
            }
            "read_fasta" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => crate::bio::read_fasta(s, line, col),
                    other => Err(type_err("read_fasta", "a string path", other, line, col)),
                }
            }
            "read_fastq" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Str(s) => crate::bio::read_fastq(s, line, col),
                    other => Err(type_err("read_fastq", "a string path", other, line, col)),
                }
            }
            // ---- tensor constructors ----
            "tensor" => {
                arity(name, &args, 1, line, col)?;
                Ok(Value::Tensor(Rc::new(tensor::from_value(&args[0], line, col)?)))
            }
            "dataframe" => {
                // Build a DataFrame from in-memory columns: `dataframe({age: [30, 41],
                // name: ["a", "b"]})`. Each record field is a column (its array); the
                // backend seam (`build_frame`) checks equal lengths / duplicate names.
                arity(name, &args, 1, line, col)?;
                let fields = match &args[0] {
                    Value::Record(f) => f,
                    other => {
                        return Err(type_err("dataframe", "a record of columns", other, line, col)
                            .hint("e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`."))
                    }
                };
                if fields.is_empty() {
                    return Err(HelixError::new("`dataframe` needs at least one column", line, col)
                        .hint("e.g. `dataframe({age: [30, 41], name: [\"a\", \"b\"]})`."));
                }
                let mut columns = Vec::with_capacity(fields.len());
                for (sym, val) in fields.iter() {
                    let cname = sym.as_str();
                    columns.push((cname.to_string(), array_to_coldata(cname, val, line, col)?));
                }
                Ok(Value::dataframe(crate::backend::build_frame(columns, line, col)?))
            }
            "zeros" | "ones" => {
                arity(name, &args, 1, line, col)?;
                let shape = tensor_shape_arg(&args[0], line, col)?;
                // Guard the element count (checked, so the product can't overflow
                // and ask ndarray for an absurd allocation that aborts).
                const MAX_ELEMS: usize = 1_000_000_000; // ~8 GB of f64
                let count = shape.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d));
                if !matches!(count, Some(c) if c <= MAX_ELEMS) {
                    return Err(HelixError::new(
                        format!("tensor shape {:?} is too large to allocate", shape),
                        line,
                        col,
                    )
                    .hint("the total element count must stay under 1 billion."));
                }
                let t = if name == "zeros" {
                    tensor::zeros(&shape)
                } else {
                    tensor::ones(&shape)
                };
                Ok(Value::Tensor(Rc::new(t)))
            }
            "eye" => {
                arity(name, &args, 1, line, col)?;
                let n = as_int(&args[0], "eye", line, col)?;
                if n < 0 {
                    return Err(HelixError::new("`eye` needs a non-negative size", line, col));
                }
                // Guard the n*n element count (an `eye(40000)` is ~12.8 GB and would
                // OOM-abort), matching the `zeros`/`ones` cap.
                let n = n as usize;
                if !matches!(n.checked_mul(n), Some(c) if c <= 1_000_000_000) {
                    return Err(HelixError::new(
                        format!("`eye({n})` is too large to allocate"),
                        line,
                        col,
                    )
                    .hint("the total element count (n*n) must stay under 1 billion."));
                }
                Ok(Value::Tensor(Rc::new(tensor::eye(n))))
            }
            // ---- reverse-mode autodiff (src/autodiff.rs) ----
            "variable" => {
                arity(name, &args, 1, line, col)?;
                crate::autodiff::variable(&args[0], line, col)
            }
            "value_of" => {
                arity(name, &args, 1, line, col)?;
                Ok(crate::autodiff::value_of(&args[0]))
            }
            "gradient" => {
                arity(name, &args, 2, line, col)?;
                crate::autodiff::gradient(&args[0], &args[1], line, col)
            }
            // ---- math standard library (broadcasts over arrays, propagates missing) ----
            "sqrt" | "cbrt" | "exp" | "ln" | "log10" | "log2" | "sin" | "cos" | "tan" | "asin"
            | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "degrees" | "radians" | "erf"
            | "normal_cdf" | "normal_pdf" | "relu" | "sigmoid" => {
                arity(name, &args, 1, line, col)?;
                // A tracked (autodiff) argument builds a graph node instead.
                if matches!(&args[0], Value::Node(_)) {
                    return crate::autodiff::unary_builtin(name, &args[0], line, col);
                }
                let f: fn(f64) -> f64 = match name {
                    "erf" => crate::stats::erf,
                    "normal_cdf" => crate::stats::normal_cdf,
                    "normal_pdf" => crate::stats::normal_pdf,
                    "sqrt" => f64::sqrt,
                    "cbrt" => f64::cbrt,
                    "exp" => f64::exp,
                    "ln" => f64::ln,
                    "log10" => f64::log10,
                    "log2" => f64::log2,
                    "sin" => f64::sin,
                    "cos" => f64::cos,
                    "tan" => f64::tan,
                    "asin" => f64::asin,
                    "acos" => f64::acos,
                    "atan" => f64::atan,
                    "sinh" => f64::sinh,
                    "cosh" => f64::cosh,
                    "tanh" => f64::tanh,
                    "degrees" => f64::to_degrees,
                    "radians" => f64::to_radians,
                    "relu" => |x: f64| x.max(0.0),
                    "sigmoid" => |x: f64| 1.0 / (1.0 + (-x).exp()),
                    _ => unreachable!(),
                };
                // Pass the operand by value so a unique buffer can be reused in place.
                apply_float_fn(name, f, args.into_iter().next().unwrap(), line, col)
            }
            // Index of the largest / smallest element (first on ties) — over an array
            // or a tensor (flattened). The classification companion to `softmax`.
            "argmax" | "argmin" => {
                arity(name, &args, 1, line, col)?;
                let want_max = name == "argmax";
                let vals: Vec<f64> = match &args[0] {
                    Value::Array(items) => {
                        let vs = items.to_values();
                        // Three-valued propagation (ADR 0001): a `missing` or `NaN`
                        // element makes the arg-extreme undefined, so return `missing`
                        // — matching sum/mean/min/max/median — instead of raising a
                        // type error on `missing` or silently skipping a `NaN`.
                        if vs
                            .iter()
                            .any(|v| matches!(v, Value::Missing) || matches!(v, Value::Float(f) if f.is_nan()))
                        {
                            return Ok(Value::Missing);
                        }
                        let mut out = Vec::with_capacity(vs.len());
                        for v in vs.iter() {
                            out.push(
                                v.as_f64()
                                    .ok_or_else(|| type_err(name, "an array of numbers", v, line, col))?,
                            );
                        }
                        out
                    }
                    Value::Tensor(t) => {
                        if t.iter().any(|f| f.is_nan()) {
                            return Ok(Value::Missing);
                        }
                        t.iter().copied().collect()
                    }
                    other => {
                        return Err(type_err(name, "an array or tensor of numbers", other, line, col))
                    }
                };
                if vals.is_empty() {
                    return Err(HelixError::new(
                        format!("`{name}` of an empty collection"),
                        line,
                        col,
                    ));
                }
                let mut best = 0usize;
                for (i, &x) in vals.iter().enumerate() {
                    if (want_max && x > vals[best]) || (!want_max && x < vals[best]) {
                        best = i;
                    }
                }
                Ok(Value::Int(best as i64))
            }
            "floor" | "ceil" | "trunc" => {
                arity(name, &args, 1, line, col)?;
                let f: fn(f64) -> f64 = match name {
                    "floor" => f64::floor,
                    "ceil" => f64::ceil,
                    "trunc" => f64::trunc,
                    _ => unreachable!(),
                };
                apply_round_fn(name, f, &args[0], line, col)
            }
            "round" => {
                // `round(x)` → nearest integer (Int); `round(x, d)` → round to `d`
                // decimal places (Float). Both broadcast over arrays and pass `missing`.
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`round` takes a number and an optional digit count, got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                if args.len() == 1 {
                    return apply_round_fn("round", f64::round, &args[0], line, col);
                }
                let d = as_int(&args[1], "round", line, col)?;
                // Clamp the digit count into f64's decimal-exponent span before the
                // `as i32` narrowing. Without the clamp, a large `d` like `2^31` wraps
                // to `i32::MIN`, so `10^d` underflows to 0 and every result is `0/0`
                // = NaN. Beyond ~10^±308 the scale is inf/0 anyway, so nothing is lost.
                let scale = 10f64.powi(d.clamp(-308, 308) as i32);
                broadcast_unary(&args[0], &|s| match s {
                    Value::Int(i) => Ok(Value::Float(*i as f64)),
                    Value::Float(x) => {
                        // Rounding to more precision than the scaled value can hold (so
                        // `x * scale` overflows to ±inf) is a no-op — return `x`
                        // unchanged rather than the inf/inf = NaN the formula would give.
                        let r = (x * scale).round() / scale;
                        Ok(Value::Float(if r.is_finite() { r } else { *x }))
                    }
                    other => Err(type_err("round", "a number or array of numbers", other, line, col)),
                })
            }
            "abs" => {
                arity(name, &args, 1, line, col)?;
                // A tracked (autodiff) argument builds a graph node — same routing as
                // the unary-float family above; `abs` lives in its own arm only because
                // it also handles Ints.
                if matches!(&args[0], Value::Node(_)) {
                    return crate::autodiff::unary_builtin(name, &args[0], line, col);
                }
                // `wrapping_abs` matches the wrapping-on-overflow convention used by the
                // arithmetic ops; a packed array maps over its buffer (no per-element box).
                super::apply_abs(args.into_iter().next().unwrap(), line, col)
            }
            "sign" => {
                arity(name, &args, 1, line, col)?;
                super::apply_sign(&args[0], line, col)
            }
            // IEEE float predicates → Bool (Bool array / 0.0-or-1.0 tensor mask when
            // broadcast). An `Int`/`Rational` is always finite, never NaN/inf. These let a
            // program guard a `NaN`/`inf` (e.g. from an `exp` overflow) BEFORE a comparison
            // — which raises on a non-orderable `NaN` rather than silently mis-ordering.
            "is_nan" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_nan, false, line, col)
            }
            "is_finite" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_finite, true, line, col)
            }
            "is_infinite" => {
                arity(name, &args, 1, line, col)?;
                float_predicate(&args[0], name, f64::is_infinite, false, line, col)
            }
            // Monotonic seconds since the first call (process start), for `t0 =
            // clock_monotonic()` … `clock_monotonic() - t0` timing. Impure + monotonic
            // (never decreases); the absolute value is meaningless, only differences are.
            "clock_monotonic" => {
                arity(name, &args, 0, line, col)?;
                use std::sync::OnceLock;
                use std::time::Instant;
                static START: OnceLock<Instant> = OnceLock::new();
                Ok(Value::Float(START.get_or_init(Instant::now).elapsed().as_secs_f64()))
            }
            "log" => match args.len() {
                // single-arg log(x) = natural log (numpy parity); broadcasts + missing
                1 => apply_float_fn("log", f64::ln, args.into_iter().next().unwrap(), line, col),
                2 => match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((x, base)) => Ok(Value::Float(x.log(base))),
                },
                _ => Err(HelixError::new(
                    format!("`log` takes 1 or 2 arguments, got {}", args.len()),
                    line,
                    col,
                )
                .hint("`log(x)` is the natural log; `log(x, base)` is log to a base.")),
            }
            "atan2" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((y, x)) => Ok(Value::Float(y.atan2(x))),
                }
            }
            "hypot" => {
                arity(name, &args, 2, line, col)?;
                match two_nums(name, &args[0], &args[1], line, col)? {
                    None => Ok(Value::Missing),
                    Some((a, b)) => Ok(Value::Float(a.hypot(b))),
                }
            }
            "gcd" => {
                arity(name, &args, 2, line, col)?;
                match (&args[0], &args[1]) {
                    (Value::Missing, _) | (_, Value::Missing) => Ok(Value::Missing),
                    _ => {
                        let a = as_int(&args[0], "gcd", line, col)?;
                        let b = as_int(&args[1], "gcd", line, col)?;
                        Ok(Value::Int(gcd_i64(a, b)))
                    }
                }
            }
            "primes" => {
                // All primes below n as a packed Int array — the native Sieve of
                // Eratosthenes. The sieve is an inherently MUTABLE algorithm Helix's
                // immutable surface cannot express efficiently (functional trial
                // division is O(N√N)); like the tensor `.matmul()`, it delegates to
                // Rust. `primes(10000000).count()` = 664579 in ~sieve time, not ~90 s.
                arity(name, &args, 1, line, col)?;
                if matches!(args[0], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let n = as_int(&args[0], "primes", line, col)?;
                if n > 100_000_000 {
                    return Err(HelixError::new(
                        format!("`primes` supports n up to 100000000, got {n}"),
                        line,
                        col,
                    )
                    .hint("the sieve buffer grows with n; sieve in segments for larger bounds."));
                }
                Ok(Value::int_array(sieve_primes(n)))
            }
            "isqrt" => {
                arity(name, &args, 1, line, col)?;
                if matches!(args[0], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let n = as_int(&args[0], "isqrt", line, col)?;
                if n < 0 {
                    return Err(HelixError::new(
                        format!(
                            "`isqrt` got {n}, but the integer square root is undefined for a negative number"
                        ),
                        line,
                        col,
                    )
                    .hint("isqrt(n) = floor(sqrt(n)) and needs n >= 0."));
                }
                Ok(Value::Int(isqrt_i64(n)))
            }
            "chr" => {
                arity(name, &args, 1, line, col)?;
                if matches!(args[0], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let cp = as_int(&args[0], "chr", line, col)?;
                match u32::try_from(cp).ok().and_then(char::from_u32) {
                    Some(c) => Ok(Value::Str(Rc::new(c.to_string()))),
                    None => Err(HelixError::new(
                        format!("`chr` got {cp}, which is not a valid Unicode codepoint"),
                        line,
                        col,
                    )
                    .hint("pass 0..=1114111 (0x10FFFF), excluding the surrogate range.")),
                }
            }
            "ord" => {
                arity(name, &args, 1, line, col)?;
                match &args[0] {
                    Value::Missing => Ok(Value::Missing),
                    // The codepoint of the FIRST character (forgiving on longer strings).
                    Value::Str(s) | Value::Dna(s) => match s.chars().next() {
                        Some(c) => Ok(Value::Int(c as i64)),
                        None => Err(HelixError::new("`ord` got an empty string", line, col)
                            .hint("pass a one-character string, e.g. `ord(\"A\")`.")),
                    },
                    other => Err(type_err("ord", "a single-character string", other, line, col)),
                }
            }
            "rational" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let n = as_int(&args[0], "rational", line, col)?;
                let d = as_int(&args[1], "rational", line, col)?;
                if d == 0 {
                    return Err(HelixError::new("`rational` denominator cannot be zero", line, col));
                }
                Ok(Value::Rational(Rc::new(num_rational::BigRational::new(n.into(), d.into()))))
            }
            "numerator" | "denominator" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Rational(r) => {
                        let big = if name == "numerator" { r.numer() } else { r.denom() };
                        big.to_i64().map(Value::Int).ok_or_else(|| {
                            HelixError::new(format!("`{name}` is too large for an integer"), line, col)
                        })
                    }
                    Value::Int(i) => Ok(Value::Int(if name == "numerator" { *i } else { 1 })),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err(name, "a rational or integer", other, line, col)),
                }
            }
            "to_float" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Int(i) => Ok(Value::Float(*i as f64)),
                    Value::Float(f) => Ok(Value::Float(*f)),
                    Value::Rational(r) => Ok(Value::Float(r.to_f64().unwrap_or(f64::NAN))),
                    // A numeric *string* is parsed (the honest spelling of what people
                    // used to reach for `parse_json` to do).
                    Value::Str(s) => parse_str_float(s, line, col),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err("to_float", "a number or numeric string", other, line, col)),
                }
            }
            "dict" => {
                // An empty keyed map (ADR 0020); build a populated one with
                // `[(k, v), …].to_dict()`. Grows via the immutable `.insert(k, v)`.
                if !args.is_empty() {
                    return Err(HelixError::new(
                        "`dict()` takes no arguments",
                        line,
                        col,
                    )
                    .hint("build a populated dict with `[(k, v), …].to_dict()` or `xs.frequencies().to_dict()`."));
                }
                Ok(Value::Dict(Rc::new(std::collections::BTreeMap::new())))
            }
            "to_int" => {
                arity(name, &args, 1, line, col)?;
                use num_traits::ToPrimitive;
                match &args[0] {
                    Value::Int(i) => Ok(Value::Int(*i)),
                    // Truncate toward zero — matches the usual numeric→int narrowing.
                    Value::Float(f) => Ok(Value::Int(f.trunc() as i64)),
                    Value::Rational(r) => Ok(Value::Int(r.to_f64().unwrap_or(f64::NAN).trunc() as i64)),
                    Value::Str(s) => parse_str_int(s, line, col),
                    Value::Missing => Ok(Value::Missing),
                    other => Err(type_err("to_int", "a number or integer string", other, line, col)),
                }
            }
            "lll" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`lll` takes a basis and an optional delta, got {}", args.len()),
                        line,
                        col,
                    )
                    .hint("e.g. `lll(basis)` or `lll(basis, 0.99)`."));
                }
                let rows = match &args[0] {
                    Value::Array(outer) => outer,
                    other => return Err(type_err("lll", "an array of basis vectors", other, line, col)),
                };
                let mut basis: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
                for row in rows.to_values().iter() {
                    match row {
                        Value::Array(inner) => {
                            let mut v = Vec::with_capacity(inner.len());
                            for x in inner.to_values().iter() {
                                match x.as_f64() {
                                    Some(f) => v.push(f),
                                    None => {
                                        return Err(type_err("lll", "numeric basis entries", x, line, col))
                                    }
                                }
                            }
                            basis.push(v);
                        }
                        other => {
                            return Err(type_err("lll", "each basis vector to be an array", other, line, col))
                        }
                    }
                }
                let delta = match args.get(1) {
                    Some(d) => d
                        .as_f64()
                        .ok_or_else(|| type_err("lll", "a numeric delta", &args[1], line, col))?,
                    None => 0.75,
                };
                let reduced =
                    crate::lattice::lll(basis, delta).map_err(|e| HelixError::new(e, line, col))?;
                let out: Vec<Value> = reduced.into_iter().map(Value::float_array).collect();
                Ok(Value::array(out))
            }
            "lll_exact" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(HelixError::new(
                        format!("`lll_exact` takes a basis and an optional delta, got {}", args.len()),
                        line,
                        col,
                    )
                    .hint("e.g. `lll_exact(basis)` or `lll_exact(basis, 0.99)`."));
                }
                let rows = match &args[0] {
                    Value::Array(outer) => outer,
                    other => {
                        return Err(type_err("lll_exact", "an array of basis vectors", other, line, col))
                    }
                };
                // Coerce every entry to an exact integer (Int, or integer-valued
                // Float/Rational); a fractional entry is a clean error — exact LLL is
                // defined on an integer lattice.
                let mut basis: Vec<Vec<num_bigint::BigInt>> = Vec::with_capacity(rows.len());
                for row in rows.to_values().iter() {
                    match row {
                        Value::Array(inner) => {
                            let mut v = Vec::with_capacity(inner.len());
                            for x in inner.to_values().iter() {
                                v.push(num_bigint::BigInt::from(as_int(x, "lll_exact", line, col)?));
                            }
                            basis.push(v);
                        }
                        other => {
                            return Err(type_err(
                                "lll_exact",
                                "each basis vector to be an array",
                                other,
                                line,
                                col,
                            ))
                        }
                    }
                }
                let delta = match args.get(1) {
                    Some(d) => d
                        .as_f64()
                        .ok_or_else(|| type_err("lll_exact", "a numeric delta", &args[1], line, col))?,
                    None => 0.75,
                };
                let reduced = crate::lattice::lll_exact(basis, delta)
                    .map_err(|e| HelixError::new(e, line, col))?;
                use num_traits::ToPrimitive;
                let mut out: Vec<Value> = Vec::with_capacity(reduced.len());
                for row in reduced {
                    let mut iv = Vec::with_capacity(row.len());
                    for x in row {
                        let i = x.to_i64().ok_or_else(|| {
                            HelixError::new(
                                "`lll_exact` reduced-basis entry overflows a 64-bit integer",
                                line,
                                col,
                            )
                        })?;
                        iv.push(Value::Int(i));
                    }
                    out.push(Value::array(iv));
                }
                Ok(Value::array(out))
            }
            "align" => {
                if args.len() < 2 || args.len() > 4 {
                    return Err(HelixError::new(
                        format!(
                            "`align` takes (a, b), (a, b, mode), or (a, b, mode, scoring), got {} arguments",
                            args.len()
                        ),
                        line,
                        col,
                    )
                    .hint("e.g. `align(a, b)`, `align(a, b, \"local\")`, or `align(a, b, \"global\", {match: 2, mismatch: -3, gap_open: -5, gap_extend: -1})`."));
                }
                let seq = |v: &Value| -> Result<Vec<Value>, HelixError> {
                    match v {
                        Value::Array(arr) => Ok(arr.to_values().into_owned()),
                        other => Err(type_err("align", "an array", other, line, col)),
                    }
                };
                let a = seq(&args[0])?;
                let b = seq(&args[1])?;
                // The Gotoh DP fills six O(n·m) tables (~27 bytes/cell at i64 scores);
                // cap the cell count so a huge pair fails cleanly instead of trying to
                // allocate gigabytes.
                const MAX_ALIGN_CELLS: u128 = 50_000_000;
                if (a.len() as u128 + 1) * (b.len() as u128 + 1) > MAX_ALIGN_CELLS {
                    return Err(HelixError::new(
                        "`align` sequences are too large (the alignment matrix would exceed 50M cells)",
                        line,
                        col,
                    ));
                }
                // Optional trailing args, in either order: a mode string and/or a
                // scoring record. Scoring defaults to match +1 / mismatch −1 / no
                // gap-open / gap-extend −1; any field may be overridden.
                let mut mode = crate::align::Mode::Global;
                let mut sc =
                    crate::align::Scoring { match_: 1, mismatch: -1, gap_open: 0, gap_extend: -1 };
                let (mut mode_set, mut scoring_set) = (false, false);
                for extra in &args[2..] {
                    match extra {
                        Value::Str(s) => {
                            if mode_set {
                                return Err(HelixError::new("`align` was given two modes", line, col));
                            }
                            mode = match s.as_str() {
                                "global" => crate::align::Mode::Global,
                                "local" => crate::align::Mode::Local,
                                "semiglobal" => crate::align::Mode::Semiglobal,
                                other => {
                                    return Err(HelixError::new(
                                        format!("unknown align mode `{other}`"),
                                        line,
                                        col,
                                    )
                                    .hint("use \"global\", \"local\", or \"semiglobal\"."))
                                }
                            };
                            mode_set = true;
                        }
                        Value::Record(fields) => {
                            if scoring_set {
                                return Err(HelixError::new(
                                    "`align` was given two scoring records",
                                    line,
                                    col,
                                ));
                            }
                            for (k, v) in fields.iter() {
                                let iv = as_int(v, "align scoring", line, col)?;
                                if !(-1_000_000..=1_000_000).contains(&iv) {
                                    return Err(HelixError::new(
                                        "`align` scoring values must be within ±1000000",
                                        line,
                                        col,
                                    ));
                                }
                                match k.as_str() {
                                    "match" => sc.match_ = iv,
                                    "mismatch" => sc.mismatch = iv,
                                    "gap_open" => sc.gap_open = iv,
                                    "gap_extend" => sc.gap_extend = iv,
                                    other => {
                                        return Err(HelixError::new(
                                            format!("unknown align scoring field `{other}`"),
                                            line,
                                            col,
                                        )
                                        .hint("scoring fields: match, mismatch, gap_open, gap_extend."))
                                    }
                                }
                            }
                            scoring_set = true;
                        }
                        other => {
                            return Err(type_err(
                                "align",
                                "a mode string or a scoring record",
                                other,
                                line,
                                col,
                            ))
                        }
                    }
                }
                let (score, steps) =
                    crate::align::align_path(a.len(), b.len(), mode, sc, |i, j| values_equal(&a[i], &b[j]));
                let mut a_al = Vec::with_capacity(steps.len());
                let mut b_al = Vec::with_capacity(steps.len());
                let mut matches = 0i64;
                for st in &steps {
                    match st {
                        crate::align::Step::Both(ia, ib) => {
                            if values_equal(&a[*ia], &b[*ib]) {
                                matches += 1;
                            }
                            a_al.push(a[*ia].clone());
                            b_al.push(b[*ib].clone());
                        }
                        crate::align::Step::OnlyA(ia) => {
                            a_al.push(a[*ia].clone());
                            b_al.push(Value::Missing);
                        }
                        crate::align::Step::OnlyB(ib) => {
                            a_al.push(Value::Missing);
                            b_al.push(b[*ib].clone());
                        }
                    }
                }
                Ok(Value::Record(Rc::new(vec![
                    (Symbol::intern("score"), Value::Int(score)),
                    (Symbol::intern("matches"), Value::Int(matches)),
                    (Symbol::intern("length"), Value::Int(steps.len() as i64)),
                    (Symbol::intern("a_aligned"), Value::array(a_al)),
                    (Symbol::intern("b_aligned"), Value::array(b_al)),
                ])))
            }
            "min" | "max" => {
                arity(name, &args, 2, line, col)?;
                if matches!(args[0], Value::Missing) || matches!(args[1], Value::Missing) {
                    return Ok(Value::Missing);
                }
                let a = args[0]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[0], line, col))?;
                let b = args[1]
                    .as_f64()
                    .ok_or_else(|| type_err(name, "a number", &args[1], line, col))?;
                let pick_first = if name == "min" { a <= b } else { a >= b };
                Ok(if pick_first { args[0].clone() } else { args[1].clone() })
            }
            "clamp" => {
                // `clamp(x, lo, hi)` — the scalar companion to the array `.clamp(lo, hi)` method,
                // mirroring `min`/`max`: it returns one of the three ORIGINAL values (so an `Int`
                // stays `Int`), never coercing to `Float`. `lo > hi` is a caller error.
                arity(name, &args, 3, line, col)?;
                if args.iter().any(|a| matches!(a, Value::Missing)) {
                    return Ok(Value::Missing);
                }
                let x = args[0].as_f64().ok_or_else(|| type_err(name, "a number", &args[0], line, col))?;
                let lo = args[1].as_f64().ok_or_else(|| type_err(name, "a number", &args[1], line, col))?;
                let hi = args[2].as_f64().ok_or_else(|| type_err(name, "a number", &args[2], line, col))?;
                if lo > hi {
                    return Err(HelixError::new(
                        format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                        line,
                        col,
                    )
                    .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
                }
                Ok(if x < lo {
                    args[1].clone()
                } else if x > hi {
                    args[2].clone()
                } else {
                    args[0].clone()
                })
            }
            "correlation" => {
                arity(name, &args, 2, line, col)?;
                // `missing` in either series propagates (ADR-0001); a non-array, a
                // length mismatch, or a non-numeric element is a clean error.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                if xs.len() != ys.len() {
                    return Err(HelixError::new(
                        format!(
                            "`correlation` needs two equal-length arrays, got {} and {}",
                            xs.len(),
                            ys.len()
                        ),
                        line,
                        col,
                    ));
                }
                if xs.is_empty() {
                    return Err(HelixError::new(
                        "cannot compute `correlation` of empty arrays",
                        line,
                        col,
                    ));
                }
                match crate::stats::pearson(&xs, &ys) {
                    Some(r) => Ok(Value::Float(r)),
                    None => Err(HelixError::new(
                        "correlation is undefined: one of the series has zero variance",
                        line,
                        col,
                    )
                    .hint("a constant series has no spread to correlate.")),
                }
            }
            "t_test" => {
                arity(name, &args, 2, line, col)?;
                // Welch's two-sample t-test → {statistic, df, p_value}. `missing` in
                // either sample propagates; each needs at least two values.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                match crate::stats::welch_t_test(&xs, &ys) {
                    Some((t, df, p)) => {
                        let fields = vec![
                            (Symbol::intern("statistic"), Value::Float(t)),
                            (Symbol::intern("df"), Value::Float(df)),
                            (Symbol::intern("p_value"), Value::Float(p)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "t-test is undefined: each sample needs at least two values with spread",
                        line,
                        col,
                    )
                    .hint("two constant samples have no variance to compare.")),
                }
            }
            "linear_regression" => {
                arity(name, &args, 2, line, col)?;
                // OLS fit of `y ~ x` → {slope, intercept, r_squared, slope_std_error,
                // slope_p_value}. `missing` in either series propagates.
                let xs = num_array(name, &args[0], line, col)?;
                let ys = num_array(name, &args[1], line, col)?;
                let (xs, ys) = match (xs, ys) {
                    (Some(xs), Some(ys)) => (xs, ys),
                    _ => return Ok(Value::Missing),
                };
                if xs.len() != ys.len() {
                    return Err(HelixError::new(
                        format!(
                            "`linear_regression` needs two equal-length arrays, got {} and {}",
                            xs.len(),
                            ys.len()
                        ),
                        line,
                        col,
                    ));
                }
                let floats = |xs: Vec<f64>| Value::float_array(xs);
                match crate::stats::linear_regression(&xs, &ys) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("slope"), Value::Float(f.slope)),
                            (Symbol::intern("intercept"), Value::Float(f.intercept)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("slope_std_error"), Value::Float(f.slope_std_error)),
                            (Symbol::intern("slope_p_value"), Value::Float(f.slope_p_value)),
                            (Symbol::intern("rss"), Value::Float(f.rss)),
                            (Symbol::intern("predictions"), floats(f.predictions)),
                            (Symbol::intern("residuals"), floats(f.residuals)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "linear regression is undefined: need at least three points and variance in both x and y",
                        line,
                        col,
                    )
                    .hint("a constant predictor or response has no line to fit.")),
                }
            }
            "multiple_regression" => {
                // OLS fit of `y` on several predictor columns. args: (predictors, y)
                // with an optional 3rd boolean `intercept` (default true). `missing`
                // anywhere propagates. coefficients/std_errors/p_values are
                // parameter-indexed arrays (with an intercept, index 0 is it).
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`multiple_regression` takes (predictors, y[, intercept]), got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let with_intercept = match args.get(2) {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(type_err("multiple_regression", "a boolean `intercept` flag", other, line, col));
                    }
                };
                let preds = num_arrays(name, &args[0], line, col)?;
                let y = num_array(name, &args[1], line, col)?;
                let (preds, y) = match (preds, y) {
                    (Some(preds), Some(y)) => (preds, y),
                    _ => return Ok(Value::Missing),
                };
                let floats = |xs: Vec<f64>| Value::float_array(xs);
                match crate::stats::multiple_regression(&preds, &y, with_intercept) {
                    Some(f) => {
                        let fields = vec![
                            (Symbol::intern("coefficients"), floats(f.coefficients)),
                            (Symbol::intern("std_errors"), floats(f.std_errors)),
                            (Symbol::intern("p_values"), floats(f.p_values)),
                            (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                            (Symbol::intern("adj_r_squared"), Value::Float(f.adj_r_squared)),
                            (Symbol::intern("rss"), Value::Float(f.rss)),
                            (Symbol::intern("predictions"), floats(f.predictions)),
                            (Symbol::intern("residuals"), floats(f.residuals)),
                        ];
                        Ok(Value::Record(Rc::new(fields)))
                    }
                    None => Err(HelixError::new(
                        "multiple regression is undefined: need more observations than predictors, equal-length non-collinear predictors, and variance in y",
                        line,
                        col,
                    )
                    .hint("e.g. `multiple_regression([x1, x2], y)` with enough rows.")),
                }
            }
            "least_squares" => {
                // OLS without inference (no std_errors/p_values) — the fast fit for
                // model selection. args: (predictors, y[, intercept]).
                if args.len() < 2 || args.len() > 3 {
                    return Err(HelixError::new(
                        format!("`least_squares` takes (predictors, y[, intercept]), got {}", args.len()),
                        line,
                        col,
                    ));
                }
                let with_intercept = match args.get(2) {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(type_err("least_squares", "a boolean `intercept` flag", other, line, col));
                    }
                };
                let preds = num_arrays(name, &args[0], line, col)?;
                let y = num_array(name, &args[1], line, col)?;
                let (preds, y) = match (preds, y) {
                    (Some(preds), Some(y)) => (preds, y),
                    _ => return Ok(Value::Missing),
                };
                match crate::stats::least_squares(&preds, &y, with_intercept) {
                    Some(f) => Ok(Value::Record(Rc::new(vec![
                        (Symbol::intern("coefficients"), Value::float_array(f.coefficients)),
                        (Symbol::intern("rss"), Value::Float(f.rss)),
                        (Symbol::intern("r_squared"), Value::Float(f.r_squared)),
                        (Symbol::intern("predictions"), Value::float_array(f.predictions)),
                        (Symbol::intern("residuals"), Value::float_array(f.residuals)),
                    ]))),
                    None => Err(HelixError::new(
                        "least squares is undefined: need at least as many rows as parameters, equal-length non-collinear predictors",
                        line,
                        col,
                    )
                    .hint("e.g. `least_squares([x1, x2], y)`.")),
                }
            }
            "linspace" => {
                // `linspace(start, stop, n)` → n evenly-spaced floats, endpoints
                // inclusive (the float analogue of `range`, for sampling a domain).
                arity(name, &args, 3, line, col)?;
                let f = |v: &Value| -> Result<f64, HelixError> {
                    v.as_f64().ok_or_else(|| type_err("linspace", "a number", v, line, col))
                };
                let (start, stop) = (f(&args[0])?, f(&args[1])?);
                let n = as_int(&args[2], "linspace", line, col)?;
                if n < 0 {
                    return Err(HelixError::new("`linspace` needs a non-negative count", line, col));
                }
                if n as usize > 100_000_000 {
                    return Err(HelixError::new("`linspace` count is too large (limit 100M)", line, col));
                }
                let n = n as usize;
                let out: Vec<f64> = match n {
                    0 => Vec::new(),
                    1 => vec![start],
                    _ => (0..n)
                        .map(|i| start + (i as f64) * (stop - start) / ((n - 1) as f64))
                        .collect(),
                };
                Ok(Value::float_array(out))
            }
            // Model-evaluation metrics over two equal-length numeric arrays. `missing`
            // in either propagates (ADR 0001).
            "mse" | "rmse" | "mae" | "r2_score" => {
                arity(name, &args, 2, line, col)?;
                let a = num_array(name, &args[0], line, col)?;
                let b = num_array(name, &args[1], line, col)?;
                let (a, b) = match (a, b) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Ok(Value::Missing),
                };
                if a.len() != b.len() {
                    return Err(HelixError::new(
                        format!("`{name}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
                        line,
                        col,
                    ));
                }
                if a.is_empty() {
                    return Err(HelixError::new(format!("cannot compute `{name}` of empty arrays"), line, col));
                }
                let nf = a.len() as f64;
                match name {
                    "mse" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf)),
                    "rmse" => Ok(Value::Float((a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / nf).sqrt())),
                    "mae" => Ok(Value::Float(a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / nf)),
                    // r2_score(y_true, y_pred) = 1 - SS_res/SS_tot — y_true first, to
                    // match scikit-learn and the sibling metrics (mse/mae take y_true,
                    // y_pred). SS_tot is the variance of the *true* values.
                    _ => {
                        let (actual, pred) = (&a, &b);
                        let mean = actual.iter().sum::<f64>() / nf;
                        let ss_tot: f64 = actual.iter().map(|v| (v - mean).powi(2)).sum();
                        if ss_tot == 0.0 {
                            return Err(HelixError::new(
                                "`r2_score` is undefined: the actual values have zero variance",
                                line,
                                col,
                            ));
                        }
                        let ss_res: f64 = actual.iter().zip(pred).map(|(y, p)| (y - p).powi(2)).sum();
                        Ok(Value::Float(1.0 - ss_res / ss_tot))
                    }
                }
            }
            // Information criteria for model selection (Gaussian-likelihood form):
            // aic(rss, n, k) = n*ln(rss/n) + 2k ; bic(rss, n, k) = n*ln(rss/n) + k*ln(n).
            "aic" | "bic" => {
                arity(name, &args, 3, line, col)?;
                let rss = args[0].as_f64().ok_or_else(|| type_err(name, "a number (rss)", &args[0], line, col))?;
                let nn = as_int(&args[1], name, line, col)?;
                let kk = as_int(&args[2], name, line, col)?;
                if nn <= 0 {
                    return Err(HelixError::new(format!("`{name}` needs n > 0"), line, col));
                }
                if rss < 0.0 {
                    return Err(HelixError::new(format!("`{name}` needs rss >= 0"), line, col));
                }
                let (nf, kf) = (nn as f64, kk as f64);
                let log_like = nf * (rss / nf).ln(); // ∝ -2·logL up to a constant
                Ok(Value::Float(if name == "aic" {
                    log_like + 2.0 * kf
                } else {
                    log_like + kf * nf.ln()
                }))
            }
            // Classification metrics — free functions taking (y_true, y_pred), mirroring
            // the regression metrics above. `accuracy` is multiclass-safe; the rest are
            // binary against a positive class (3rd arg `pos_label`, default `1`).
            "accuracy" => {
                arity(name, &args, 2, line, col)?;
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let correct = a.iter().zip(&b).filter(|(x, y)| values_equal(x, y)).count();
                Ok(Value::Float(correct as f64 / a.len() as f64))
            }
            "precision" | "recall" | "f1_score" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(HelixError::new(
                        format!("`{name}` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                let (tp, fp, fa_neg, _) = binary_counts(&a, &b, &pos);
                // sklearn convention: an undefined ratio (no predicted/actual positives)
                // is reported as 0.0 rather than raising.
                let precision = if tp + fp == 0 { 0.0 } else { tp as f64 / (tp + fp) as f64 };
                let recall = if tp + fa_neg == 0 { 0.0 } else { tp as f64 / (tp + fa_neg) as f64 };
                Ok(Value::Float(match name {
                    "precision" => precision,
                    "recall" => recall,
                    _ if precision + recall == 0.0 => 0.0,
                    _ => 2.0 * precision * recall / (precision + recall),
                }))
            }
            "confusion_matrix" => {
                if args.len() != 2 && args.len() != 3 {
                    return Err(HelixError::new(
                        format!("`confusion_matrix` takes (y_true, y_pred) or (y_true, y_pred, pos_label), got {} arguments", args.len()),
                        line,
                        col,
                    ));
                }
                let (a, b) = label_pair(name, &args[0], &args[1], line, col)?;
                let pos = if args.len() == 3 { args[2].clone() } else { Value::Int(1) };
                let (tp, fp, fa_neg, tn) = binary_counts(&a, &b, &pos);
                Ok(Value::Record(Rc::new(vec![
                    (Symbol::intern("tp"), Value::Int(tp)),
                    (Symbol::intern("fp"), Value::Int(fp)),
                    (Symbol::intern("fn"), Value::Int(fa_neg)),
                    (Symbol::intern("tn"), Value::Int(tn)),
                ])))
            }
            _ => {
                let err =
                    HelixError::new(format!("`{}` is not a known function", name), line, col);
                Err(match crate::suggest::hint(name, crate::suggest::Site::Function, &[]) {
                    Some(h) => err.hint(h),
                    None => err,
                })
            }
        }
    }
}

/// Convert a Helix array (one column of a `dataframe({…})` call) into a
/// backend-agnostic [`ColData`]. The column type is inferred from the first
/// non-`missing` element; `missing` becomes a null (selecting the nullable
/// variant). Ints + Floats in one column promote to Float; a column may not mix
/// otherwise-incompatible types.
fn array_to_coldata(
    name: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<crate::backend::ColData, HelixError> {
    use crate::backend::ColData;
    let arr = match v {
        Value::Array(a) => a,
        other => {
            return Err(HelixError::new(
                format!("column `{}` must be an array, but got {}", name, crate::value::with_article(other.type_name())),
                line,
                col,
            )
            .hint("each DataFrame column is an array, e.g. `dataframe({age: [30, 41]})`."))
        }
    };
    let vals = arr.to_values();
    let mixed = |got: &Value| {
        HelixError::new(
            format!("column `{}` mixes types (found {})", name, crate::value::with_article(got.type_name())),
            line,
            col,
        )
        .hint("each DataFrame column must be all one type.")
    };
    let has_missing = vals.iter().any(|x| matches!(x, Value::Missing));
    let kind = match vals.iter().find(|x| !matches!(x, Value::Missing)) {
        Some(k) => k,
        None => {
            return Err(HelixError::new(
                format!("cannot infer the type of column `{}` (empty or all `missing`)", name),
                line,
                col,
            ))
        }
    };
    match kind {
        Value::Int(_) | Value::Float(_) => {
            let any_float = vals.iter().any(|x| matches!(x, Value::Float(_)));
            if any_float {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i as f64)),
                        Value::Float(f) => out.push(Some(*f)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Float(out))
            } else if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(Some(*i)),
                        Value::Missing => out.push(None),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::IntOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Int(i) => out.push(*i),
                        o => return Err(mixed(o)),
                    }
                }
                Ok(ColData::Int(out))
            }
        }
        Value::Str(_) | Value::Dna(_) => {
            let pull = |x: &Value| match x {
                Value::Str(s) => Some((**s).clone()),
                Value::Dna(s) => Some((**s).clone()),
                _ => None,
            };
            if has_missing {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match x {
                        Value::Missing => out.push(None),
                        _ => match pull(x) {
                            Some(s) => out.push(Some(s)),
                            None => return Err(mixed(x)),
                        },
                    }
                }
                Ok(ColData::StrOpt(out))
            } else {
                let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
                for x in vals.iter() {
                    match pull(x) {
                        Some(s) => out.push(s),
                        None => return Err(mixed(x)),
                    }
                }
                Ok(ColData::Str(out))
            }
        }
        Value::Bool(_) => {
            if has_missing {
                return Err(HelixError::new(
                    format!("boolean column `{}` cannot contain `missing`", name),
                    line,
                    col,
                ));
            }
            let mut out = crate::error::try_with_capacity(vals.len(), "DataFrame column", line, col)?;
            for x in vals.iter() {
                match x {
                    Value::Bool(b) => out.push(*b),
                    o => return Err(mixed(o)),
                }
            }
            Ok(ColData::Bool(out))
        }
        other => Err(HelixError::new(
            format!("column `{}` has an unsupported element type ({})", name, other.type_name()),
            line,
            col,
        )
        .hint("DataFrame columns must be numbers, strings, DNA, or booleans.")),
    }
}

/// Extract a slice of numeric columns from an array-of-arrays argument (the predictor
/// matrix of `multiple_regression`). Returns `Ok(None)` if any element anywhere is
/// `missing`; errors if the outer value is not an array, or any inner value is not a
/// numeric array.
fn num_arrays(
    who: &str,
    v: &Value,
    line: usize,
    col: usize,
) -> Result<Option<Vec<Vec<f64>>>, HelixError> {
    let outer = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of predictor arrays", other, line, col)),
    };
    let mut cols = Vec::with_capacity(outer.len());
    for el in outer.to_values().iter() {
        match num_array(who, el, line, col)? {
            Some(c) => cols.push(c),
            None => return Ok(None),
        }
    }
    Ok(Some(cols))
}

/// Greatest common divisor of two i64 (Euclid). Sign-agnostic; gcd(n,0)=|n|.
/// Computed in i128 so `abs(i64::MIN)` doesn't overflow.
fn gcd_i64(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = ((a as i128).unsigned_abs(), (b as i128).unsigned_abs());
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a as i64
}

/// All primes below `n` (empty for `n <= 2`) via a byte-array Sieve of Eratosthenes —
/// strikes start at `p*p` (no usize overflow: `p < n <= 1e8` keeps `p*p < 1e16`).
/// O(n log log n) time, O(n) bytes for the composite flags.
fn sieve_primes(n: i64) -> Vec<i64> {
    if n <= 2 {
        return Vec::new();
    }
    let n = n as usize;
    let mut composite = vec![false; n];
    let mut out = Vec::new();
    for p in 2..n {
        if !composite[p] {
            out.push(p as i64);
            let mut m = p * p;
            while m < n {
                composite[m] = true;
                m += p;
            }
        }
    }
    out
}

/// Integer square root: the largest `x >= 0` with `x*x <= n`. Caller guarantees `n >= 0`.
/// An f64 seed corrected in i128 — exact even near `i64::MAX`, where `sqrt() as i64` can be
/// off by one and the verification `x*x` would overflow i64.
fn isqrt_i64(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    let n = n as i128;
    let mut x = (n as f64).sqrt() as i128;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x as i64
}

/// Parse an even-length hex string (`0-9a-fA-F`) into its bytes; `None` on odd length or a
/// non-hex digit. Shared by `hex_decode` and the AES key parser.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let hb = s.as_bytes();
    if !hb.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hb.len() / 2);
    for pair in hb.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Parse a 32-byte (64-hex-char) AES-256 key, with a clear error otherwise.
fn aes_key_bytes(hex: &str, line: usize, col: usize) -> Result<Vec<u8>, HelixError> {
    match hex_to_bytes(hex) {
        Some(b) if b.len() == 32 => Ok(b),
        _ => Err(HelixError::new(
            "an AES key must be 64 hex characters (32 bytes) — generate one with `aes_keygen()`",
            line,
            col,
        )),
    }
}

/// Parse exactly 32 bytes from a 64-hex string into a fixed array (Ed25519 keys), with a
/// `what`-labelled error otherwise.
fn hex_to_array32(hex: &str, what: &str, line: usize, col: usize) -> Result<[u8; 32], HelixError> {
    match hex_to_bytes(hex).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
        Some(a) => Ok(a),
        None => Err(HelixError::new(format!("{what} must be 64 hex characters (32 bytes)"), line, col)),
    }
}

/// Extract `(method, url, body, headers)` from an `http_request`/`http_stream` request record
/// (`{method, url, body?, headers?}`). `headers` may be a Record (identifier names), a Dict,
/// or an array of `[name, value]` pairs (the inline-friendly form for dash-named headers,
/// since Helix has no dict literal). Shared by both client verbs.
/// The parsed parts of an http request record: `(method, url, body, headers)`.
type HttpReqParts = (String, String, String, Vec<(String, String)>);

#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn http_request_fields(req: &Value, line: usize, col: usize) -> Result<HttpReqParts, HelixError> {
    let Value::Record(fields) = req else {
        return Err(type_err("http_request", "a `{ method, url, … }` record", req, line, col));
    };
    let field = |k: &str| fields.iter().find(|(s, _)| s.as_str() == k).map(|(_, v)| v);
    let str_field = |v: &Value, what: &str| -> Result<String, HelixError> {
        match v {
            Value::Str(s) => Ok((**s).clone()),
            other => Err(type_err("http_request", what, other, line, col)),
        }
    };
    let method = match field("method") {
        Some(v) => str_field(v, "a string `method`")?.to_uppercase(),
        None => {
            return Err(HelixError::new("the request record needs a `method` field", line, col)
                .hint("e.g. `{method: \"PUT\", url: u, body: b}`"));
        }
    };
    let url = match field("url") {
        Some(v) => str_field(v, "a string `url`")?,
        None => return Err(HelixError::new("the request record needs a `url` field", line, col)),
    };
    let body = match field("body") {
        Some(v) => str_field(v, "a string `body`")?,
        None => String::new(),
    };
    let hval = |v: &Value| match v {
        Value::Str(s) => (**s).clone(),
        other => other.to_string(),
    };
    let mut hdrs: Vec<(String, String)> = Vec::new();
    match field("headers") {
        Some(Value::Record(hf)) => {
            for (k, v) in hf.iter() {
                hdrs.push((k.as_str().to_string(), hval(v)));
            }
        }
        Some(Value::Dict(map)) => {
            for (k, v) in map.iter() {
                if let crate::value::DictKey::Str(s) = k {
                    hdrs.push(((**s).clone(), hval(v)));
                }
            }
        }
        Some(Value::Array(items)) => {
            for it in items.to_values().iter() {
                let two: Vec<Value> = match it {
                    Value::Array(a) => a.to_values().to_vec(),
                    Value::Tuple(t) => t.iter().cloned().collect(),
                    _ => continue,
                };
                if let [Value::Str(k), v] = two.as_slice() {
                    hdrs.push(((**k).clone(), hval(v)));
                }
            }
        }
        _ => {}
    }
    Ok((method, url, body, hdrs))
}

/// Read the optional `timeout_ms` field from an `http_stream` request record — a positive
/// integer per-chunk read deadline in milliseconds. Absent → `None` (no read timeout). A
/// non-integer or non-positive value is a clean error rather than a silently-ignored field.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn http_timeout_ms(req: &Value, line: usize, col: usize) -> Result<Option<u64>, HelixError> {
    let Value::Record(fields) = req else { return Ok(None) };
    match fields.iter().find(|(s, _)| s.as_str() == "timeout_ms").map(|(_, v)| v) {
        None | Some(Value::Missing) => Ok(None),
        Some(Value::Int(n)) if *n > 0 => Ok(Some(*n as u64)),
        Some(_) => Err(HelixError::new(
            "`timeout_ms` must be a positive integer (milliseconds)",
            line,
            col,
        )
        .hint("e.g. `http_stream({method: \"POST\", url: u, body: b, timeout_ms: 5000})`.")),
    }
}

/// Read a whole file to a `String`, capping at `MAX_STRING_LEN` so a huge file is a
/// clean error rather than an allocator abort. Shared by `read_text` and `read_json`.
fn read_text_file(path: &str, line: usize, col: usize) -> Result<String, HelixError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        HelixError::new(format!("could not read `{path}`: {e}"), line, col)
            .hint("check the path exists and is readable.")
    })?;
    if meta.len() as usize > crate::interp::MAX_STRING_LEN {
        return Err(HelixError::new(
            format!("`{path}` is larger than the {}-byte text limit", crate::interp::MAX_STRING_LEN),
            line,
            col,
        )
        .hint("read very large tabular files with read_csv/read_parquet instead."));
    }
    std::fs::read_to_string(path)
        .map_err(|e| HelixError::new(format!("could not read `{path}`: {e}"), line, col))
}

/// Extract two equal-length, non-empty label arrays as raw `Value`s for the
/// classification metrics. Labels are compared by value equality — never coerced to
/// f64 — so integer (0/1), boolean, and string class labels all work uniformly.
fn label_pair(
    who: &str,
    yt: &Value,
    yp: &Value,
    line: usize,
    col: usize,
) -> Result<(Vec<Value>, Vec<Value>), HelixError> {
    let take = |v: &Value| -> Result<Vec<Value>, HelixError> {
        match v {
            Value::Array(items) => Ok(items.to_values().into_owned()),
            other => Err(type_err(who, "an array of labels", other, line, col)),
        }
    };
    let (a, b) = (take(yt)?, take(yp)?);
    if a.len() != b.len() {
        return Err(HelixError::new(
            format!("`{who}` needs two equal-length arrays, got {} and {}", a.len(), b.len()),
            line,
            col,
        ));
    }
    if a.is_empty() {
        return Err(HelixError::new(format!("cannot compute `{who}` of empty arrays"), line, col));
    }
    Ok((a, b))
}

/// Tally (tp, fp, fn, tn) of `pred` against `truth` for one positive class `pos`.
fn binary_counts(truth: &[Value], pred: &[Value], pos: &Value) -> (i64, i64, i64, i64) {
    let (mut tp, mut fp, mut fa_neg, mut tn) = (0i64, 0i64, 0i64, 0i64);
    for (yt, yp) in truth.iter().zip(pred) {
        match (values_equal(yt, pos), values_equal(yp, pos)) {
            (true, true) => tp += 1,
            (false, true) => fp += 1,
            (true, false) => fa_neg += 1,
            (false, false) => tn += 1,
        }
    }
    (tp, fp, fa_neg, tn)
}

/// Extract a numeric `Vec<f64>` from an array argument. Returns `Ok(None)` when any
/// element is `missing` *or* a `NaN` float (so the caller can propagate `missing`,
/// per ADR-0001 — a `NaN` would otherwise silently corrupt the bivariate result),
/// and errors if the value is not an array or holds a non-numeric element.
fn num_array(who: &str, v: &Value, line: usize, col: usize) -> Result<Option<Vec<f64>>, HelixError> {
    let items = match v {
        Value::Array(items) => items,
        other => return Err(type_err(who, "an array of numbers", other, line, col)),
    };
    let mut out = Vec::with_capacity(items.len());
    for el in items.to_values().iter() {
        match el {
            Value::Missing => return Ok(None),
            Value::Float(f) if f.is_nan() => return Ok(None),
            _ => match el.as_f64() {
                Some(x) => out.push(x),
                None => return Err(type_err(who, "an array of numbers", el, line, col)),
            },
        }
    }
    Ok(Some(out))
}

/// Parse a string to a float for `to_float(s)` and `String.to_float()`. Leading/trailing
/// whitespace is ignored; a non-numeric string is a clear error (not a silent NaN).
pub(crate) fn parse_str_float(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<f64>().map(Value::Float).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as a number"), line, col)
            .hint("`to_float` expects a numeric string like \"3.14\" or \"-2\".")
    })
}

/// Parse a string to an integer for `to_int(s)` and `String.to_int()`. Strict: a decimal
/// string like "3.5" is rejected (use `to_float`), so an integer field never rounds silently.
pub(crate) fn parse_str_int(s: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    let t = s.trim();
    t.parse::<i64>().map(Value::Int).map_err(|_| {
        HelixError::new(format!("could not parse {t:?} as an integer"), line, col)
            .hint("`to_int` expects an integer string like \"42\" or \"-7\"; for decimals use `to_float`.")
    })
}

/// An IEEE float predicate (`is_nan`/`is_finite`/`is_infinite`): `Float` → `Bool` via `f`;
/// an `Int`/`Rational` is exact so it yields `on_exact` (always finite, never NaN/inf);
/// `missing` propagates; an array maps elementwise to a `Bool` array; a tensor (f64-only,
/// so no `Bool` element type) yields a `1.0`/`0.0` mask tensor usable in arithmetic.
fn float_predicate(
    v: &Value,
    name: &str,
    f: fn(f64) -> bool,
    on_exact: bool,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match v {
        Value::Array(items) => {
            let out: Result<Vec<Value>, HelixError> = items
                .to_values()
                .iter()
                .map(|e| float_predicate(e, name, f, on_exact, line, col))
                .collect();
            Ok(Value::array(out?))
        }
        Value::Tensor(t) => {
            let data: Vec<f64> = t.iter().map(|&x| if f(x) { 1.0 } else { 0.0 }).collect();
            let out = ndarray::ArrayD::from_shape_vec(t.raw_dim(), data)
                .expect("same length as source tensor");
            Ok(Value::Tensor(Rc::new(out)))
        }
        Value::Missing => Ok(Value::Missing),
        Value::Float(x) => Ok(Value::Bool(f(*x))),
        Value::Int(_) | Value::Rational(_) => Ok(Value::Bool(on_exact)),
        other => Err(type_err(name, "a number or array of numbers", other, line, col)),
    }
}
