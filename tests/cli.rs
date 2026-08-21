//! End-to-end CLI integration tests: they run the *actual compiled `helix`
//! binary* as a subprocess, exercising the parts unit tests can't reach — argument
//! parsing, file reading, exit codes, stdout/stderr, the REPL, and the
//! `HELIX_NOVM` engine switch. Cargo provides the freshly-built binary path via
//! `CARGO_BIN_EXE_helix`, and runs with the package root as the working directory
//! so the examples' relative data paths (`examples/data/*.csv`) resolve.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the `helix` binary and capture (stdout, stderr, exit_code). `env` adds
/// environment variables; `stdin` is fed to the process (for the REPL).
fn run(args: &[&str], env: &[(&str, &str)], stdin: &str) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_helix"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn helix");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("failed to wait on helix");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// Run a source string by writing it to a unique temp file (tests run in
/// parallel, so the name is tagged).
fn run_source(src: &str, env: &[(&str, &str)], tag: &str) -> (String, String, Option<i32>) {
    let path = std::env::temp_dir().join(format!("helix_it_{tag}.helix"));
    std::fs::write(&path, src).unwrap();
    let r = run(&[path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_file(&path);
    r
}

/// The runnable, self-contained example categories. Excludes `examples/{data,
/// modules,python,api}`: data holds fixtures (not programs), modules is an
/// import demo, and python/api need optional features / network.
fn example_files() -> Vec<std::path::PathBuf> {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut out = Vec::new();
    for cat in ["language", "numerics", "dataframes", "statistics", "bio"] {
        let dir = base.join(cat);
        for e in std::fs::read_dir(&dir).unwrap_or_else(|_| panic!("examples/{cat}/ dir")) {
            let p = e.unwrap().path();
            if p.extension().and_then(|s| s.to_str()) == Some("helix") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_example_runs_clean() {
    let files = example_files();
    assert!(files.len() >= 10, "expected the example suite, saw {}", files.len());
    for path in files {
        let rel = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_str().unwrap();
        let (stdout, stderr, code) = run(&[rel], &[], "");
        assert_eq!(code, Some(0), "`{rel}` exited {code:?}; stderr:\n{stderr}");
        assert!(!stdout.trim().is_empty(), "`{rel}` produced no output");
    }
}

/// The VM (default) and the tree-walker (`HELIX_NOVM=1`) must produce identical
/// output for every example — the same parity the differential fuzzers check at
/// the unit level, here through the real CLI. `dataframes.helix` is excluded: its
/// group-by emits rows in Polars' nondeterministic order.
#[test]
fn vm_matches_tree_walker_via_cli() {
    for path in example_files() {
        let name = path.file_name().unwrap().to_str().unwrap();
        // Excluded: group-by emits rows in Polars' nondeterministic order, so the two
        // engines can print the same rows in a different order.
        if name == "dataframes.helix" || name == "variants.helix" {
            continue;
        }
        let rel = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_str().unwrap();
        let (vm, _, vc) = run(&[rel], &[], "");
        let (tw, _, tc) = run(&[rel], &[("HELIX_NOVM", "1")], "");
        assert_eq!(vc, Some(0), "VM run of `{rel}` failed");
        assert_eq!(tc, Some(0), "tree-walker run of `{rel}` failed");
        assert_eq!(vm, tw, "VM and tree-walker disagree on `{rel}`");
    }
}

#[test]
fn dataframe_vstack_appends_rows() {
    // Same columns → rows append (3 = 2 + 1); both engines agree.
    let src = "a = dataframe({id: [1, 2], v: [10.0, 20.0]})\n\
               b = dataframe({id: [3], v: [30.0]})\n\
               print(a.vstack(b).count())\n";
    for env in [&[][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "vstack_ok");
        assert_eq!(code, Some(0), "stderr: {err}");
        assert_eq!(out.trim(), "3");
    }
    // Mismatched columns are a clean error (caught by `try`), not a silent null-fill.
    let bad = "a = dataframe({id: [1]})\n\
               c = dataframe({other: [9]})\n\
               print((try a.vstack(c)).ok)\n";
    let (out, _, code) = run_source(bad, &[], "vstack_bad");
    assert_eq!(code, Some(0));
    assert_eq!(out.trim(), "false");
}

#[test]
fn text_and_json_io_round_trip() {
    let dir = std::env::temp_dir().join("helix_io_rt");
    std::fs::create_dir_all(&dir).unwrap();
    let txt = dir.join("hello.txt");
    let jsn = dir.join("data.json");
    std::fs::write(&txt, "hello helix").unwrap();
    std::fs::write(&jsn, "{\"a\": 1, \"b\": [2, 3]}").unwrap();
    let src = format!(
        "print(file_exists(\"{t}\"))\n\
         print(read_text(\"{t}\"))\n\
         j = read_json(\"{j}\")\n\
         print(j.a, j.b[1])\n\
         print(file_exists(\"{t}.nope\"))\n",
        t = txt.to_str().unwrap(),
        j = jsn.to_str().unwrap(),
    );
    let (out, err, code) = run_source(&src, &[], "io_rt");
    assert_eq!(code, Some(0), "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "true"); // file_exists
    assert_eq!(lines[1], "hello helix"); // read_text
    assert_eq!(lines[2], "1 3"); // read_json field + index
    assert_eq!(lines[3], "false"); // missing file
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn emit_writes_plain_flushed_lines() {
    // `emit` is the streaming sink: one PLAIN line per value (no rich/grouped formatting,
    // unlike `print`), flushed immediately. `emit(x.to_json())` is the NDJSON wire format.
    let (out, stderr, code) = run(
        &["eval", "emit(\"hi\")\nemit(1000000)\nemit({a: 1, b: 2}.to_json())"],
        &[],
        "",
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // strings unquoted; integers ungrouped (1000000, not 1,000,000); records as NDJSON.
    assert_eq!(out, "hi\n1000000\n{\"a\":1,\"b\":2}\n", "got: {out:?}");
    // wrong arity is a clear error (one value = one line).
    let (_, _, bad) = run(&["eval", "emit(1, 2)"], &[], "");
    assert_eq!(bad, Some(1));
}

#[test]
fn sleep_runs_and_validates() {
    // `sleep` paces a loop (timing not asserted to avoid flakiness — just that it runs and
    // emits stream correctly around it). `sleep(0)` is a no-op; negatives / non-numbers err.
    let (out, stderr, code) =
        run(&["eval", "sleep(0)\nemit(\"a\")\nsleep(1)\nemit(\"b\")"], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out, "a\nb\n");
    assert_eq!(run(&["eval", "sleep(0 - 1)"], &[], "").2, Some(1)); // negative
    assert_eq!(run(&["eval", "sleep(\"x\")"], &[], "").2, Some(1)); // non-number
}

#[test]
fn raw_strings_and_chr_ord() {
    // Triple-quoted raw string: braces are literal (no interpolation), and quotes go in
    // verbatim — the fix for the brace-doubling wart (CSS/JSON/regex).
    let (out, err, code) = run(&["eval", "print(\"\"\"x{a}y\"\"\")"], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(out, "x{a}y\n");
    // JSON with braces AND quotes, literal.
    assert_eq!(run(&["eval", "print(\"\"\"{\"k\": 1}\"\"\")"], &[], "").0, "{\"k\": 1}\n");
    // chr/ord round trip, including a non-ASCII codepoint.
    assert_eq!(run(&["eval", "print(chr(65))"], &[], "").0, "A\n");
    assert_eq!(run(&["eval", "print(ord(\"A\"))"], &[], "").0, "65\n");
    assert_eq!(run(&["eval", "print(chr(955))"], &[], "").0, "\u{3bb}\n"); // λ
    assert_eq!(run(&["eval", "print(ord(\"\u{3bb}\"))"], &[], "").0, "955\n");
    // An invalid codepoint is a clean error.
    assert_eq!(run(&["eval", "print(chr(0 - 1))"], &[], "").2, Some(1));
}

#[test]
fn crypto_hmac_and_base64() {
    // HMAC-SHA256 against the canonical RFC vector — proves real byte-level HMAC.
    assert_eq!(
        run(
            &["eval", "print(hmac_sha256(\"key\", \"The quick brown fox jumps over the lazy dog\"))"],
            &[],
            "",
        )
        .0,
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8\n"
    );
    // base64 encode/decode round trip (with padding).
    assert_eq!(run(&["eval", "print(base64_encode(\"hello\"))"], &[], "").0, "aGVsbG8=\n");
    assert_eq!(run(&["eval", "print(base64_decode(\"aGVsbG8=\"))"], &[], "").0, "hello\n");
    // Invalid base64 is a clean error (not a silent wrong answer).
    assert_eq!(run(&["eval", "print(base64_decode(\"!@#\"))"], &[], "").2, Some(1));
}

#[test]
fn crypto_aes_and_hex() {
    // hex round trip.
    assert_eq!(run(&["eval", "print(hex_encode(\"hi\"))"], &[], "").0, "6869\n");
    assert_eq!(run(&["eval", "print(hex_decode(\"6869\"))"], &[], "").0, "hi\n");
    // AES-256-GCM round trip with a fixed key (encrypt's nonce is random; decrypt recovers).
    let k = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let prog = format!("key = \"{k}\"\nprint(aes_decrypt(key, aes_encrypt(key, \"secret message\")))");
    assert_eq!(run(&["eval", &prog], &[], "").0, "secret message\n");
    // A wrong-length key is a clean error.
    assert_eq!(run(&["eval", "print(aes_encrypt(\"abc\", \"x\"))"], &[], "").2, Some(1));
    // The wrong key fails to decrypt — authenticated, never silent garbage.
    let k2 = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    let wrong = format!(
        "b = aes_encrypt(\"{k}\", \"x\")\nr = try aes_decrypt(\"{k2}\", b)\nprint(r.ok)"
    );
    assert_eq!(run(&["eval", &wrong], &[], "").0, "false\n");
}

#[test]
fn crypto_ed25519_sign_verify() {
    // Sign with the private key, verify with the matching public key — the round trip holds.
    let ok = "k = ed25519_keygen()\ns = ed25519_sign(k.private, \"hello\")\nprint(ed25519_verify(k.public, \"hello\", s))";
    assert_eq!(run(&["eval", ok], &[], "").0, "true\n");
    // A tampered message verifies false (not an error).
    let tampered = "k = ed25519_keygen()\ns = ed25519_sign(k.private, \"hello\")\nprint(ed25519_verify(k.public, \"HELLO\", s))";
    assert_eq!(run(&["eval", tampered], &[], "").0, "false\n");
    // A malformed signature verifies false (not an error).
    let bad = "k = ed25519_keygen()\nprint(ed25519_verify(k.public, \"x\", \"00\"))";
    assert_eq!(run(&["eval", bad], &[], "").0, "false\n");
    // A wrong-length key is a clean error.
    assert_eq!(run(&["eval", "print(ed25519_sign(\"abcd\", \"x\"))"], &[], "").2, Some(1));
}

#[test]
fn build_produces_runnable_standalone_exe() {
    let dir = std::env::temp_dir();
    let src = dir.join("helix_build_ok.helix");
    std::fs::write(&src, "x = 21\nprint(x * 2)\n").unwrap();
    let exe = dir.join("helix_build_ok_out");
    let _ = std::fs::remove_file(&exe);

    // Build the standalone executable.
    let (out, stderr, code) =
        run(&["build", src.to_str().unwrap(), "-o", exe.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "build failed; stderr:\n{stderr}");
    assert!(out.contains("standalone"), "unexpected build output: {out}");
    assert!(exe.exists(), "build produced no executable");

    // Run the produced exe directly — it must execute the embedded program with no
    // args and no `helix` on PATH (we invoke it by absolute path).
    let produced = Command::new(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the produced executable");
    let res = produced.wait_with_output().expect("failed to wait on produced exe");
    assert_eq!(res.status.code(), Some(0), "produced exe stderr:\n{}", String::from_utf8_lossy(&res.stderr));
    assert_eq!(String::from_utf8_lossy(&res.stdout), "42\n");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&exe);
}

#[test]
fn http_server_serves_a_request() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // A one-shot server: bind, handle exactly one request, exit. The handler echoes
    // the request path back in a JSON body — exercising listen/accept/request/respond.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve.helix");
    std::fs::write(
        &src,
        "conn = listen(18231).accept()\n\
         conn.respond({ status: 200, json: { ok: true, echo: conn.request().path } })\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    // Wait for the listener to come up (retry the connect for ~2.5s).
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18231") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never started listening on 18231");
    stream.write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap(); // server sends Connection: close → EOF
    let _ = child.wait();

    assert!(resp.contains("200 OK"), "response:\n{resp}");
    assert!(resp.contains("application/json"), "response:\n{resp}");
    assert!(resp.contains("\"ok\":true"), "response:\n{resp}");
    assert!(resp.contains("\"echo\":\"/ping\""), "response:\n{resp}");

    let _ = std::fs::remove_file(&src);
}

#[test]
fn event_loop_server_serves_keepalive() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    // The cooperative event-loop server: `wait` for readiness, `accept_poll` new connections,
    // `poll_request`/`respond` each ready one, keep the open ones. The point is HTTP/1.1
    // keep-alive — serving *two* requests over *one* connection — which the blocking
    // accept/respond model can't do. Handler echoes the path.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_evserve.helix");
    std::fs::write(
        &src,
        "fn handle(req) = { status: 200, text: req.path }\n\
         fn evloop(l, conns) = do {\n\
           ready = l.wait(conns, 50)\n\
           fresh = l.accept_poll()\n\
           live = if fresh.is_missing() then conns else conns.concat([fresh])\n\
           active = live.filter(c => do {\n\
             req = c.poll_request()\n\
             if req.is_missing() then c.is_open() else do {\n\
               sent = c.respond(handle(req))\n\
               c.is_open()\n\
             }\n\
           })\n\
           evloop(l, active)\n\
         }\n\
         evloop(listen(18233), [])\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the event-loop server");

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18233") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("event-loop server never started listening on 18233");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    // Read one full HTTP response: headers, then Content-Length body bytes. A single
    // read() can return just the header segment (TCP splits headers from body), so loop
    // until the whole framed message has arrived.
    fn read_response(stream: &mut TcpStream) -> String {
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let head_end = acc.windows(4).position(|w| w == b"\r\n\r\n");
            if let Some(he) = head_end {
                let head = String::from_utf8_lossy(&acc[..he]).to_ascii_lowercase();
                let clen = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if acc.len() >= he + 4 + clen {
                    break;
                }
            }
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&acc).to_string()
    }

    // Two requests on ONE connection — keep-alive reuse.
    stream.write_all(b"GET /one HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let r1 = read_response(&mut stream);
    stream.write_all(b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let r2 = read_response(&mut stream);

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&src);

    // First reply: 200, keep-alive (socket stays open), echoes the path.
    assert!(r1.contains("200 OK"), "resp1:\n{r1}");
    assert!(r1.contains("keep-alive"), "resp1 should be keep-alive:\n{r1}");
    assert!(r1.contains("/one"), "resp1 body echoes path:\n{r1}");
    // Second reply on the SAME connection proves keep-alive reuse works.
    assert!(r2.contains("200 OK"), "resp2 (keep-alive reuse):\n{r2}");
    assert!(r2.contains("/two"), "resp2 body echoes path:\n{r2}");
}

#[test]
fn http_server_sends_custom_headers_and_redirects() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // A redirect: an explicit envelope `{ status, headers }` with no body — the response
    // must carry the Location header and an empty body (the gap a real framework hit).
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve_hdr.helix");
    std::fs::write(
        &src,
        "conn = listen(18235).accept()\n\
         conn.respond({ status: 302, headers: { Location: \"/new\" } })\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18235") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never came up");
    stream.write_all(b"GET /old HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let _ = child.wait();

    assert!(resp.contains("302 Found"), "response:\n{resp}");
    assert!(resp.contains("Location: /new"), "response:\n{resp}");
    assert!(resp.contains("Content-Length: 0"), "response:\n{resp}");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn http_server_streams_sse_events() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // sse() opens an event stream; send() emits framed events; the program then ends,
    // closing the socket. The client must see the headers and the `data:` frames.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve_sse.helix");
    std::fs::write(
        &src,
        // `send` JSON-encodes a record, so we avoid a literal `{` inside a Helix string
        // (which would be parsed as an interpolation hole).
        "conn = listen(18236).accept()\n\
         conn.sse()\n\
         a = conn.send({ n: 1 })\n\
         b = conn.send(\"tick\")\n\
         print(\"delivered\")\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18236") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never came up");
    stream.write_all(b"GET /live HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let _ = child.wait();

    assert!(resp.contains("text/event-stream"), "response:\n{resp}");
    assert!(resp.contains("data: {\"n\":1}"), "response:\n{resp}");
    assert!(resp.contains("data: tick"), "response:\n{resp}");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn http_server_shards_across_workers() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // `listen(port, 2)` spins up a second share-nothing worker on the same port via
    // SO_REUSEPORT. We can't observe which worker the kernel routes to, but we verify the
    // server announces sharding and serves a request correctly.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve_shard.helix");
    std::fs::write(
        &src,
        "fn srv(l) = do {\n\
         \x20 c = l.accept()\n\
         \x20 c.respond({ status: 200, text: \"sharded\" })\n\
         \x20 srv(l)\n\
         }\n\
         srv(listen(18238, 2))\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    let mut stream = None;
    for _ in 0..60 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18238") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("sharded server never came up");
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let _ = child.kill();
    let out = child.wait_with_output().expect("failed to wait on the server");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(resp.contains("200 OK") && resp.contains("sharded"), "response:\n{resp}");
    assert!(stderr.contains("2 shards"), "expected a sharding announcement; stderr:\n{stderr}");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn http_server_poll_serves_cooperatively() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // poll() is a non-blocking accept: the server loops (returning `missing` while idle)
    // until a client connects, then serves it. This is the within-core cooperative model.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve_poll.helix");
    std::fs::write(
        &src,
        "fn tick(l) = do {\n\
         \x20 c = l.poll()\n\
         \x20 if c.is_missing()\n\
         \x20 then do {\n\
         \x20   sleep(10)\n\
         \x20   tick(l)\n\
         \x20 }\n\
         \x20 else do {\n\
         \x20   c.respond({ status: 200, text: \"served\" })\n\
         \x20   0\n\
         \x20 }\n\
         }\n\
         tick(listen(18237))\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect("127.0.0.1:18237") {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never came up");
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let _ = child.wait();

    assert!(resp.contains("200 OK") && resp.contains("served"), "response:\n{resp}");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn http_server_survives_a_client_disconnect() {
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    // A looping server. The first client sends a request then drops the socket without
    // reading (a closed browser tab / health probe); the server's write fails with a
    // broken pipe, which must be a no-op — the loop has to keep serving. A second client
    // then gets a real response, proving the server survived.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_serve_robust.helix");
    std::fs::write(
        &src,
        "fn srv(l) = do {\n  c = l.accept()\n  c.respond({ status: 200, text: \"alive\" })\n  srv(l)\n}\nsrv(listen(18232))\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the server");

    let connect = || -> Option<TcpStream> {
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect("127.0.0.1:18232") {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    };

    // Rude client: send a request, then immediately drop (close) without reading.
    {
        let mut rude = connect().expect("server never came up");
        rude.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        // drop(rude) at end of scope closes the socket before the server can reply.
    }
    std::thread::sleep(Duration::from_millis(150));

    // Polite client: the server must still be alive to answer.
    let mut polite = connect().expect("server died after a client disconnect");
    polite.write_all(b"GET /b HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    polite.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut resp = String::new();
    polite.read_to_string(&mut resp).unwrap();
    let _ = child.kill();
    let _ = child.wait();

    assert!(resp.contains("200 OK") && resp.contains("alive"), "response:\n{resp}");
    let _ = std::fs::remove_file(&src);
}

#[test]
fn build_rejects_a_program_that_does_not_type_check() {
    // A broken program must fail the *build*, not produce an exe that fails when run.
    let dir = std::env::temp_dir();
    let src = dir.join("helix_build_bad.helix");
    std::fs::write(&src, "print(1 + \"x\")\n").unwrap();
    let exe = dir.join("helix_build_bad_out");
    let _ = std::fs::remove_file(&exe);

    let (_, stderr, code) =
        run(&["build", src.to_str().unwrap(), "-o", exe.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("type-check"), "expected a type-check error, got:\n{stderr}");
    assert!(!exe.exists(), "a broken program must not yield an executable");

    let _ = std::fs::remove_file(&src);
}

#[test]
fn file_lifecycle_ops() {
    let dir = std::env::temp_dir().join("helix_lifecycle");
    let _ = std::fs::remove_dir_all(&dir);
    let sub = dir.join("nested/deep");
    let f = dir.join("nested/deep/note.txt");
    let src = format!(
        "print(mkdir(\"{d}\"))\n\
         w = \"hi\".write_to(\"{f}\")\n\
         print(file_exists(\"{f}\"))\n\
         print(remove_file(\"{f}\"))\n\
         print(remove_file(\"{f}\"))\n",
        d = sub.to_str().unwrap(),
        f = f.to_str().unwrap(),
    );
    let (out, err, code) = run_source(&src, &[], "lifecycle");
    assert_eq!(code, Some(0), "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "true"); // mkdir -p created the nested dir
    assert_eq!(lines[1], "true"); // file written + exists
    assert_eq!(lines[2], "true"); // removed
    assert_eq!(lines[3], "false"); // idempotent: already gone
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deep_nesting_errors_cleanly_not_abort() {
    // Deeply-nested patterns and interpolation must hit the parse-depth guard on
    // the big parse stack - a clean exit-1 error, never a stack-overflow SIGABRT.
    let pat = format!(
        "print(match 0 {{ {}0{} => 1, _ => 2 }})",
        "(".repeat(50000),
        ")".repeat(50000)
    );
    let (_, err, code) = run_source(&pat, &[], "deep_pat");
    assert_eq!(code, Some(1), "should exit 1 cleanly; stderr: {err}");
    assert!(err.contains("too deeply"), "expected a depth error, got: {err}");

    // Nested interpolation: each hole holds a string with its own interpolation,
    // so the embedded-expression parser recurses - the depth guard must catch it.
    let mut nested = "x".to_string();
    for _ in 0..50000 {
        nested = format!("\"{{{}}}\"", nested);
    }
    let (_, err2, code2) = run_source(&format!("y = {nested}\nprint(y)"), &[], "deep_interp");
    assert_eq!(code2, Some(1), "nested interpolation should exit 1; stderr: {err2}");
    assert!(err2.contains("too deeply"), "expected a depth error, got: {err2}");
}

#[test]
fn version_and_help_flags() {
    for flag in ["--version", "-V"] {
        let (stdout, _, code) = run(&[flag], &[], "");
        assert_eq!(code, Some(0));
        assert!(stdout.contains("helix"), "`{flag}` => {stdout:?}");
    }
    for flag in ["--help", "-h"] {
        let (stdout, _, code) = run(&[flag], &[], "");
        assert_eq!(code, Some(0));
        assert!(stdout.to_lowercase().contains("usage"), "`{flag}` => {stdout:?}");
    }
}

#[test]
fn missing_file_is_a_clean_error() {
    let (_, stderr, code) = run(&["does_not_exist.helix"], &[], "");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot read"), "stderr: {stderr:?}");
}

#[test]
fn type_error_aborts_before_running() {
    // An undefined name is caught by the type checker; nothing should print.
    let (stdout, stderr, code) =
        run_source("print(\"start\")\nprint(undefined_name)\n", &[], "typeerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("not defined"), "stderr: {stderr:?}");
    // The type error fires before execution, so the earlier print never runs.
    assert!(!stdout.contains("start"), "side effects leaked: {stdout:?}");
}

#[test]
fn runtime_error_exits_nonzero() {
    let (_, stderr, code) = run_source("print(1 / 0)\n", &[], "divzero");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("division by zero"), "stderr: {stderr:?}");
}

#[test]
fn immutable_reassignment_errors_on_the_vm() {
    let (_, stderr, code) = run_source("x = 1\nx = 2\nprint(x)\n", &[], "immut");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("immutable"), "stderr: {stderr:?}");
}

#[test]
fn repl_evaluates_and_exits_on_eof() {
    // No file arg => REPL. Feed one expression, then EOF (closed stdin).
    let (stdout, _, code) = run(&[], &[], "21 + 21\n");
    assert_eq!(code, Some(0), "REPL should exit cleanly on EOF");
    assert!(stdout.contains("42"), "REPL did not echo the result: {stdout:?}");
}

/// Write `files` into a fresh temp directory and run `entry` (resolved there, so
/// the loader's sibling-import resolution works). Returns (stdout, stderr, code).
fn run_modules(
    files: &[(&str, &str)],
    entry: &str,
    env: &[(&str, &str)],
    tag: &str,
) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("helix_mod_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, src) in files {
        std::fs::write(dir.join(name), src).unwrap();
    }
    let entry_path = dir.join(entry);
    let r = run(&[entry_path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// A `where`/`filter` predicate that is not a condition must be a clean Helix error — on a
/// frame read from a FILE, which is the only shape that ever aborted.
///
/// `read_csv(f).where(1)` used to exit 134, `Aborted (core dumped)`, on all three engines,
/// uncatchable by `try` (the build is `panic = "abort"`), spilling an absolute cargo-registry
/// path and a `polars-stream` source line — after `helix check` had said `ok`. ADR-0024 says
/// that cannot happen; it did.
///
/// TWO THINGS THIS TEST GETS RIGHT THAT THE OBVIOUS VERSION DOES NOT:
///
/// 1. **It reads a file.** Every DataFrame fixture in `tests/corpus/` builds its frame with
///    `dataframe({…})`, and that eager path returned a clean error for the very same
///    predicate. The abort lived only on the lazy CSV-scan path. Written the way the existing
///    fixtures are written, this test passes while the bug remains.
/// 2. **It covers the whole family, not the literal.** `not 1`, `1 and true` and `1 + 1` all
///    aborted too, so a guard that only rejected `Lit(Int)` would leave three of six live.
///    `1.5` and `"x"` never aborted — they failed through a different engine path — and are
///    included so the fix is a single Helix diagnostic rather than one message per dtype.
///
/// The determinism assertion is the second bug: `where(@a > 0 and 1)` produced two DIFFERENT
/// error texts across runs of one engine, which made the byte-identity oracle unenforceable
/// on this path. The guard fires before the backend, so the text is now fixed.
#[test]
fn a_non_boolean_filter_predicate_is_an_error_not_an_abort() {
    let dir = std::env::temp_dir().join("helix_predguard");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let csv = dir.join("f.csv");
    std::fs::write(&csv, "a,b,flag\n1,2,true\n3,4,false\n5,6,true\n").unwrap();
    let csv_path = csv.to_str().unwrap().replace('\\', "/");

    let engines: [&[(&str, &str)]; 3] = [&[], &[("HELIX_NOJIT", "1")], &[("HELIX_NOVM", "1")]];

    // The six shapes that aborted, plus two that failed noisily through another path.
    for (i, pred) in ["1", "0", "-1", "1 + 1", "not 1", "1 and true", "1.5", "\"x\""]
        .iter()
        .enumerate()
    {
        for verb in ["where", "filter"] {
            let src =
                format!("df = read_csv(\"{csv_path}\")\nprint(df.{verb}({pred}).count())\n");
            for (e, env) in engines.iter().enumerate() {
                let (out, err, code) =
                    run_source(&src, env, &format!("predguard_{i}_{verb}_{e}"));
                // The whole point: exit 1, not a signal. `Some(1)` also excludes `None`,
                // which is what a killed-by-signal process reports.
                assert_eq!(code, Some(1), "`{verb}({pred})` engine {e}: stderr:\n{err}");
                assert!(
                    err.contains("filter predicate must be a condition"),
                    "`{verb}({pred})` engine {e} gave: {err}"
                );
                // No engine internals may reach the user (ADR-0012 says none escape the seam).
                for leak in [".cargo", "polars", "panicked at", "internal error", "SchemaMismatch"] {
                    assert!(!err.contains(leak), "`{verb}({pred})` leaked `{leak}`: {err}");
                }
                assert!(out.is_empty(), "`{verb}({pred})` printed before failing: {out:?}");
            }
        }
    }

    // Predicates that ARE conditions still work, and a bare column is still the backend's
    // business — the guard must not have turned `@flag` into a false positive.
    for (pred, want) in [("@a > 1", "2"), ("@flag", "2"), ("not @flag", "1"), ("true", "3")] {
        let src = format!("df = read_csv(\"{csv_path}\")\nprint(df.where({pred}).count())\n");
        let (out, err, code) = run_source(&src, &[], &format!("predok_{}", pred.len()));
        assert_eq!(code, Some(0), "`where({pred})` should run; stderr:\n{err}");
        assert_eq!(out.trim(), want, "`where({pred})`");
    }
    // An unknown column is still a column error, not a predicate error.
    let src = format!("df = read_csv(\"{csv_path}\")\nprint(df.where(@nope).count())\n");
    let (_, err, code) = run_source(&src, &[], "predguard_unknown_col");
    assert_eq!(code, Some(1));
    assert!(err.contains("nope"), "{err}");

    // DETERMINISM. Twelve runs, one engine, must give one outcome — this program used to
    // flip between two different error texts.
    let src = format!("df = read_csv(\"{csv_path}\")\nprint(df.where(@a > 0 and 1).count())\n");
    // The SAME tag every time — `run_source` names the temp file after it, and the file name
    // appears in the rendered error, so varying the tag would compare two different programs.
    let first = run_source(&src, &[], "predguard_det");
    for i in 1..12 {
        let again = run_source(&src, &[], "predguard_det");
        assert_eq!(again, first, "error text is not deterministic across runs (run {i})");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `count()` on a CSV it cannot read must FAIL, not answer.
///
/// A 4-line CSV with one unclosed quote used to give `.count()` == **1** with **exit 0** on
/// all three engines, while `print(df)`, `df.to_table()` and `df.column("a")` on the very
/// same handle all exited 1. Polars answers a bare `select(len())` over a CSV scan by
/// scanning the bytes for record separators and never invoking the field parser, so the
/// count outlived a file nothing else could read. In a data language, a count that lies with
/// a zero exit code is worse than any error message.
///
/// THREE THINGS THIS TEST GETS RIGHT:
///
/// 1. **It reads a real file.** The bug lives entirely on the lazy CSV-scan path; every
///    `dataframe({...})` fixture in the corpus is eager and returned the right answer all
///    along. Written the way those fixtures are written, this test passes while the bug
///    remains — exactly how the `read_csv(f).where(1)` abort survived.
/// 2. **It has a positive control.** `good.csv` must still count 3, so "make count always
///    error" does not pass.
/// 3. **It pins the ragged file as ANSWERABLE.** Extra fields on a row are something real
///    pipelines emit; `count()`, `column()` and `to_table()` all read it today and must
///    keep doing so. Only the whole-frame materializers (`print`, `cache`, `write_csv`)
///    are strict about ragged, and that split is Polars policy — recorded, not "fixed"
///    here by making `read_csv` reject the file.
#[test]
fn count_cannot_answer_for_a_csv_it_cannot_read() {
    let dir = std::env::temp_dir().join("helix_csv_honesty");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p.to_str().unwrap().replace('\\', "/")
    };
    // Three data rows, and an unclosed quote on the second: unreadable.
    let bad = write("unclosed.csv", "a,b\n1,2\n\"unclosed,3\n4,5\n");
    // Three data rows, the second carrying an extra field: readable column-wise.
    let ragged = write("ragged.csv", "a,b\n1,2\n3,4,5\n6,7\n");
    let good = write("good.csv", "a,b\n1,2\n3,4\n5,6\n");

    let engines: [(&str, &[(&str, &str)]); 3] =
        [("jit", &[]), ("vm", &[("HELIX_NOJIT", "1")]), ("tree", &[("HELIX_NOVM", "1")])];

    // Every one of these already failed on the unreadable file EXCEPT the two counts.
    let readers = [
        ("count", "print(df.count())"),
        ("select_count", "print(df.select(a).count())"),
        ("head_count", "print(df.head(2).count())"),
        ("column", "print(df.column(\"a\"))"),
        ("to_table", "print(df.to_table())"),
        ("print", "print(df)"),
    ];

    for (label, op) in readers {
        let src = format!("df = read_csv(\"{bad}\")\n{op}\n");
        let mut texts = Vec::new();
        for (ename, env) in engines {
            let (out, err, code) = run_source(&src, env, &format!("csvhonesty_{label}"));
            assert_eq!(code, Some(1), "`{label}` on an unreadable CSV ({ename}) must exit 1, \
                 got {code:?} with stdout {out:?}");
            assert!(out.trim().is_empty(), "`{label}` ({ename}) printed an answer: {out:?}");
            assert!(
                err.contains("unclosed"),
                "`{label}` ({ename}) must name the offending text; stderr:\n{err}"
            );
            texts.push(err);
        }
        // ADR-0024: the three engines must agree on the error TEXT, not just on failing.
        assert_eq!(texts[0], texts[1], "`{label}`: jit and vm disagree on the error text");
        assert_eq!(texts[0], texts[2], "`{label}`: jit and tree-walker disagree on the error text");
    }

    // POSITIVE CONTROL — a well-formed file still counts, on every engine, and the count
    // still agrees with the data. (A "count always errors" fix dies here.)
    for (ename, env) in engines {
        let src = format!(
            "df = read_csv(\"{good}\")\nprint(df.count())\nprint(df.column(\"a\").length())\n"
        );
        let (out, err, code) = run_source(&src, env, &format!("csvhonesty_good_{ename}"));
        assert_eq!(code, Some(0), "good CSV ({ename}); stderr:\n{err}");
        assert_eq!(out.trim(), "3\n3", "good CSV ({ename}) count/column disagree: {out:?}");
    }

    // THE INVARIANT, stated directly: whatever `count()` reports, it is the number of
    // values you actually get back — on a readable file, and on the ragged one too.
    for (label, path) in [("good", &good), ("ragged", &ragged)] {
        let src = format!(
            "df = read_csv(\"{path}\")\nprint(df.count() == df.column(\"a\").length())\n"
        );
        let (out, err, code) = run_source(&src, &[], &format!("csvhonesty_inv_{label}"));
        assert_eq!(code, Some(0), "{label}: stderr:\n{err}");
        assert_eq!(out.trim(), "true", "{label}: count() != column length");
    }

    // RECORDED POLICY: a ragged row is still countable and still readable column-wise.
    // If this ever starts failing, someone made `read_csv` strict — a deliberate decision
    // that rejects files real pipelines emit, and it belongs in an ADR, not a diff.
    let src = format!("df = read_csv(\"{ragged}\")\nprint(df.count())\nprint(df.column(\"a\"))\n");
    let (out, err, code) = run_source(&src, &[], "csvhonesty_ragged");
    assert_eq!(code, Some(0), "ragged CSV should still be countable; stderr:\n{err}");
    assert_eq!(out.trim(), "3\n[1, 3, 6]", "ragged CSV: {out:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The foreign-syntax diagnostics: every one names what the user was ATTEMPTING.
///
/// Before this, all of these landed on one canned line — ``each statement goes on its own
/// line; Helix has no `;` `` — on sources containing no semicolon. An adversarial sweep of
/// 1438 newcomer programs found **109 of them** in that state, which made it the largest
/// diagnostic defect in the language. A wrong hint is worse than no hint: it sends the reader
/// looking for a problem that is not there and costs them the one thing the compiler knew.
///
/// `for`, `while`, `def`, `lambda`, `return` and the rest are NOT keywords here — they lex as
/// ordinary identifiers, which is exactly why the parse dies a token later with no idea what
/// was meant. Reserving them would give a better message at the cost of breaking any program
/// that uses one as a variable name.
#[test]
fn foreign_syntax_gets_the_hint_it_actually_needs() {
    // (program, a phrase the help must contain)
    let cases = [
        ("xs = [1, 2, 3]\nfor x in xs:\n    print(x)", "no `for` loop"),
        ("i = 0\nwhile i < 3:\n    print(i)", "no `while`"),
        ("def f(x):\n    return x + 1", "fn name(a, b) = expression"),
        ("f = lambda x: x + 1", "a lambda is `x => x + 1`"),
        ("fn f(x) = return x", "nothing to return"),
        ("switch x {\n  case 1: print(1)\n}", "match x {"),
        ("var x = 1", "add `mut`"),
        ("const y = 2", "add `mut`"),
        ("x := 1\nprint(x)", "no `:=`"),
        ("v = f\"v={1}\"\nprint(v)", "already interpolate"),
        ("v = (int) 3.5\nprint(v)", "no C-style casts"),
        ("v = [x * 2 for x in [1, 2]]\nprint(v)", "no list comprehension"),
        ("v = `hi`\nprint(v)", "no template literals"),
        ("v = $x\nprint(v)", "no `$` sigil"),
    ];
    for (src, want) in cases {
        let (_, err, code) = run_source(src, &[], &format!("foreign_{}", want.len()));
        assert_eq!(code, Some(1), "should fail: {src:?}\nstderr: {err}");
        assert!(
            err.contains(want),
            "for {src:?}\n  expected the help to mention {want:?}\n  got: {err}"
        );
        // …and it must NOT get the canned semicolon line, since none of these has a `;`.
        assert!(
            !err.contains("Helix has no `;`"),
            "canned semicolon hint on a source with no semicolon: {src:?}\n{err}"
        );
    }
    // The semicolon hint is still exactly right for a source that HAS one.
    let (_, err, _) = run_source("x = 1; y = 2\n", &[], "foreign_semi");
    assert!(err.contains("Helix has no `;`"), "{err}");
}

/// A lone `\r` ends a line. Classic-Mac files, and anything that has been through a mangled
/// transfer, used to collapse into ONE line — so `fn f(x) = x * 2\rprint(f(3))` died with
/// "expected end of line after statement, found `print`" and a caret pointing into a line
/// that, in the user's editor, had ended. Thirty programs in the adversarial sweep were in
/// that state, every one with a diagnostic about a problem that did not exist.
#[test]
fn a_lone_carriage_return_ends_a_line() {
    let dir = std::env::temp_dir().join("helix_cr_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, bytes, want) in [
        ("cr.helix", "fn f(x) = x * 2\rprint(f(3))\r", "6"),
        ("crlf.helix", "x = 1\r\ny = 2\r\nprint(x + y)\r\n", "3"),
        ("lf.helix", "x = 1\ny = 2\nprint(x + y)\n", "3"),
        // Mixed, because a half-converted file is the realistic case.
        ("mixed.helix", "x = 1\ry = 2\r\nprint(x + y)\n", "3"),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        let (out, err, code) = run(&[p.to_str().unwrap()], &[], "");
        assert_eq!(code, Some(0), "{name}: stderr: {err}");
        assert_eq!(out.trim(), want, "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn module_program_runs_and_matches_engines() {
    // The committed multi-file example: shapes.helix imports geometry.helix.
    let entry = "examples/modules/shapes.helix";
    let (vm, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "modules example failed; stderr:\n{stderr}");
    assert!(vm.contains("area 12"), "unexpected output: {vm:?}");
    let (tw, _, _) = run(&[entry], &[("HELIX_NOVM", "1")], "");
    assert_eq!(vm, tw, "VM and tree-walker disagree on the modules example");
}

#[test]
fn cross_module_calls_and_local_shadowing() {
    let lib = "fn double(x) = x * 2\nexport fn quad(x) = double(double(x))\nexport N = 7\n";
    // `double` is redefined locally in main — it must shadow the module's `double`.
    let main = "import lib\nprint(lib.quad(3))\nprint(lib.N)\nfn double(x) = x + 100\nprint(double(1))\n";
    let (out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "shadow");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "12\n7\n101", "got: {out:?}"); // quad(3)=12, N=7, local double(1)=101
}

#[test]
fn qualified_call_resolves_named_args_and_defaults() {
    // Named arguments and default parameters must work through module qualification
    // `dep.f(...)` — the parser can't resolve them (the callee is in another file), so the
    // module loader does, emitting a plain positional call. This is what makes the new
    // function features usable in a library (always consumed as `lib.fn(...)`).
    let lib =
        "export fn greet(name, status = 200, loud = false) = { name: name, status: status, loud: loud }\n";
    let main = concat!(
        "import lib\n",
        "print(lib.greet(\"Ada\").status)\n",              // omitted default → 200
        "print(lib.greet(\"Ada\", 201).status)\n",         // positional override → 201
        "print(lib.greet(\"Ada\", status: 404).status)\n", // named override → 404
        "print(lib.greet(\"Ada\", loud: true).loud)\n",    // named skips the middle default → true
        "print(lib.greet(\"Ada\", loud: true).status)\n",  // ...and the skipped middle keeps 200
        "print(lib.greet(\"Ada\").name)\n",                // the required positional still binds → Ada
    );
    let (vm, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "qualnamed");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm.lines().collect::<Vec<_>>(), ["200", "201", "404", "true", "200", "Ada"]);
    // Both engines run the same rewritten positional call, so they must agree.
    let (tw, _, _) = run_modules(
        &[("lib.helix", lib), ("main.helix", main)],
        "main.helix",
        &[("HELIX_NOVM", "1")],
        "qualnamed_tw",
    );
    assert_eq!(tw, vm, "VM and tree-walker disagree on qualified named/default calls");
}

#[test]
fn qualified_call_named_arg_errors_are_precise() {
    let lib = "export fn f(a, b = 1) = a + b\n";
    // An unknown named parameter is named in the error.
    let main1 = "import lib\nprint(lib.f(1, z: 9))\n";
    let (_, e1, c1) =
        run_modules(&[("lib.helix", lib), ("main.helix", main1)], "main.helix", &[], "qnerr1");
    assert_ne!(c1, Some(0), "unknown named param must error");
    assert!(e1.contains("has no parameter named `z`"), "got: {e1}");
    // Positional + named for the same parameter is a double-bind.
    let main2 = "import lib\nprint(lib.f(1, a: 2))\n";
    let (_, e2, c2) =
        run_modules(&[("lib.helix", lib), ("main.helix", main2)], "main.helix", &[], "qnerr2");
    assert_ne!(c2, Some(0), "double-bound param must error");
    assert!(e2.contains("was given more than once"), "got: {e2}");
    // A named arg on a genuine method call (a value's method) is still rejected.
    let main3 = "import lib\nprint([1, 2, 3].map(x: 1))\n";
    let (_, e3, c3) =
        run_modules(&[("lib.helix", lib), ("main.helix", main3)], "main.helix", &[], "qnerr3");
    assert_ne!(c3, Some(0), "named arg on a method must error");
    assert!(e3.contains("named arguments are not supported on method calls"), "got: {e3}");
}

#[test]
fn module_local_fn_shadows_a_builtin() {
    // A module-local `fn` of the same name as a builtin must shadow it — even inside a
    // multi-file program, where the loader rewrites names. (`dict` is a builtin; a user
    // `fn dict(a, b, c)` here must win, not fall through to the 0-arg builtin.)
    let lib = "export fn helper(x) = x * 10\n";
    let main = "import lib\nfn dict(a, b, c) = a + b + c\nprint(dict(1, 2, 3))\nprint(lib.helper(2))\n";
    let (out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "shadowbuiltin");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n20", "got: {out:?}"); // user dict(1,2,3)=6; builtin still reachable elsewhere
}

#[test]
fn cross_module_runtime_error_points_at_the_dependency() {
    // A runtime error inside an imported module must render against that module's own
    // file and local line — not the entry file. `boom` is on line 2 of lib.helix.
    let lib = "# lib\nexport fn boom(n) = [10, 20, 30][n]\n";
    let main = "import lib\nprint(\"start\")\nprint(lib.boom(99))\n";
    let (_out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "caret");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("lib.helix:2:"), "should point at lib.helix line 2:\n{stderr}");
    assert!(stderr.contains("[10, 20, 30][n]"), "should show lib's source line:\n{stderr}");
    assert!(!stderr.contains("main.helix"), "must not point at the entry file:\n{stderr}");
}

#[test]
fn import_cycle_is_rejected() {
    let a = "import b\nprint(1)\n";
    let b = "import a\nprint(2)\n";
    let (_, stderr, code) =
        run_modules(&[("a.helix", a), ("b.helix", b)], "a.helix", &[], "cycle");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cycle"), "stderr: {stderr:?}");
}

#[test]
fn missing_module_is_a_clean_error() {
    let (_, stderr, code) =
        run_modules(&[("m.helix", "import nope\nprint(1)\n")], "m.helix", &[], "missing");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot find module"), "stderr: {stderr:?}");
}

#[test]
fn import_alias_renames_the_namespace() {
    let lib = "export fn mean2(a, b) = (a + b) / 2\nexport PI = 3\n";
    // `as st` makes the module reachable as `st`, not `stats`.
    let main = "import stats as st\nprint(st.mean2(2, 4))\nprint(st.PI)\n";
    let (out, stderr, code) =
        run_modules(&[("stats.helix", lib), ("main.helix", main)], "main.helix", &[], "alias");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3.0\n3", "got: {out:?}"); // division always yields Float
    // The bare module name is NOT in scope when aliased.
    let main_bad = "import stats as st\nprint(stats.PI)\n";
    let (_, stderr2, code2) = run_modules(
        &[("stats.helix", lib), ("main.helix", main_bad)],
        "main.helix",
        &[],
        "alias_bare",
    );
    assert_ne!(code2, Some(0), "bare name should not resolve when aliased");
    assert!(!stderr2.is_empty());
}

#[test]
fn subdirectory_import_resolves_nested_path() {
    // `import lib.stats` resolves to the nested file `lib/stats.helix`.
    let dir = std::env::temp_dir().join("helix_mod_subdir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("lib").join("stats.helix"), "export fn mean2(a, b) = (a + b) / 2\n").unwrap();
    std::fs::write(dir.join("main.helix"), "import lib.stats\nprint(stats.mean2(10, 20))\n").unwrap();
    let entry = dir.join("main.helix");
    let (vm, stderr, code) = run(&[entry.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm.trim(), "15.0", "got: {vm:?}"); // division always yields Float
    // Both engines agree.
    let (tw, _, _) = run(&[entry.to_str().unwrap()], &[("HELIX_NOVM", "1")], "");
    assert_eq!(vm, tw, "VM and tree-walker disagree on a subdirectory import");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_subcommands_work() {
    // `helix eval "<code>"`
    let (out, _, code) = run(&["eval", "print(6 * 7)"], &[], "");
    assert_eq!(code, Some(0), "eval failed");
    assert_eq!(out.trim(), "42");

    // `helix version`
    let (vout, _, vcode) = run(&["version"], &[], "");
    assert_eq!(vcode, Some(0));
    assert!(vout.contains("helix"), "version: {vout:?}");

    // `helix run <file>` matches the bare-path shorthand.
    let path = std::env::temp_dir().join("helix_cli_run.helix");
    std::fs::write(&path, "print(\"hi\")\n").unwrap();
    let (rout, _, rcode) = run(&["run", path.to_str().unwrap()], &[], "");
    let _ = std::fs::remove_file(&path);
    assert_eq!(rcode, Some(0));
    assert_eq!(rout.trim(), "hi");
}

#[cfg(not(feature = "managed"))]
#[test]
fn python_subcommand_without_managed_feature_errors() {
    // A default build still parses `helix python …` but explains how to enable it.
    let (_, stderr, code) = run(&["python", "install"], &[], "");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("managed-runtime support"), "stderr: {stderr:?}");
}

#[cfg(feature = "managed")]
#[test]
fn python_dir_prints_the_managed_runtime_path() {
    // Offline command — no download. (`install` needs network, so it isn't tested here.)
    let (out, stderr, code) = run(&["python", "dir"], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(out.contains("helix"), "expected a .../helix/python path, got: {out:?}");
}

#[test]
fn json_round_trips_through_the_cli() {
    // Build a record (no string braces → no interpolation snag), serialize, re-parse,
    // and access fields — exercises to_json + parse_json + record access end to end.
    let src = "r = {a: 1, b: [2, 3]}\ns = r.to_json()\nprint(s)\nd = s.parse_json()\nprint(d.a)\nprint(d.b.sum())\n";
    let (out, stderr, code) = run_source(src, &[], "json");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "{\"a\":1,\"b\":[2,3]}");
    assert_eq!(lines[1], "1"); // d.a
    assert_eq!(lines[2], "5"); // d.b.sum()
}

#[test]
fn write_streams_tokens_inline_without_newlines() {
    // `write(x)` is the no-newline sibling of `emit` — tokens flow across one line
    // (live-chat streaming) instead of one-per-line. A trailing `emit("")` closes the
    // line. VM and tree-walker must agree byte-for-byte.
    let src = "_ = [\"He\", \"ll\", \"o\"].map(t => write(t))\nemit(\"\")\nemit(\"done\")\n";
    let (vm, stderr, code) = run_source(src, &[], "writes");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm, "Hello\ndone\n"); // 3 tokens on one line, then a break, then done
    let (tw, _, tc) = run_source(src, &[("HELIX_NOVM", "1")], "writes_tw");
    assert_eq!(tc, Some(0));
    assert_eq!(tw, vm, "tree-walker disagrees with VM on write()");
}

#[test]
fn elog_writes_to_stderr_leaving_stdout_clean() {
    // `elog(x)` streams to stderr, so a program can emit results on stdout and progress
    // on stderr without the two interleaving when stdout is piped.
    let src = "elog(\"progress\")\nemit(\"result\")\n";
    let (out, err, code) = run_source(src, &[], "elogs");
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(out, "result\n", "stdout must carry only the result");
    assert!(err.contains("progress"), "stderr must carry the log; got: {err:?}");
    assert!(!out.contains("progress"), "the log must not leak onto stdout");
}

#[test]
fn calls_a_function_value_from_an_expression() {
    // A function stored in a record field or an array is a first-class value; a
    // parenthesized call target `(expr)(args)` invokes it. This is the dispatch-table
    // pattern (route a name/index to a handler) without binding to a temp first.
    let src = concat!(
        "handlers = {double: (x => x * 2), inc: (x => x + 1)}\n",
        "print((handlers.double)(21))\n", // 42
        "fns = [(x => x + 100), (x => x - 100)]\n",
        "print((fns[0])(5))\n",           // 105
        // Not callable → a clear error (caught here so the CLI exits 0).
        "bad = {v: 3}\n",
        "print((try (bad.v)(1)).ok)\n",   // false
    );
    let (out, stderr, code) = run_source(src, &[], "callvalue");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "42");
    assert_eq!(lines[1], "105");
    assert_eq!(lines[2], "false");
}

#[test]
fn deep_tail_recursion_runs_in_constant_space() {
    // A tail-recursive loop far deeper than VM_MAX_DEPTH (1,000,000) must complete — tail-call
    // optimization reuses the frame, so the accept-loop / state-machine idiom that a real
    // server relies on no longer leaks a frame per iteration or overflows the depth limit.
    let src = "fn count(n, acc) = if n <= 0 then acc else count(n - 1, acc + 1)\nprint(count(2500000, 0))\n";
    let (out, err, code) = run_source(src, &[], "tco_deep");
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(out.trim(), "2500000");
}

#[test]
fn record_update_spread_derives_a_modified_record() {
    // `{ ...base, k: v }` is the clean way to derive a changed record from an immutable
    // one (add a header, bump a status) — override existing fields, append new ones, and
    // leave the base untouched. This is the response-composition primitive for a web layer.
    let src = concat!(
        "resp = { status: 200, body: \"ok\" }\n",
        "err = { ...resp, status: 500 }\n",
        "print(err.status)\n",                       // 500 (overridden)
        "print(err.body)\n",                         // ok (carried over)
        "withhdr = { ...resp, cookie: \"tok\" }\n",
        "print(withhdr.cookie)\n",                   // tok (appended)
        "print(withhdr.status)\n",                   // 200 (carried)
        "print(resp.status)\n",                      // 200 (base unmutated)
    );
    let (out, err, code) = run_source(src, &[], "recupdate");
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(out.lines().collect::<Vec<_>>(), ["500", "ok", "tok", "200", "200"]);
    // A non-record spread base is a clear error.
    let (_, e2, c2) = run_source("x = { ...5, a: 1 }\nprint(x)\n", &[], "recupdate_err");
    assert_ne!(c2, Some(0));
    assert!(e2.contains("record update needs a record"), "got: {e2}");
}

#[test]
fn interpolation_hole_resolves_named_args_and_defaults() {
    // A `{expr}` interpolation hole is parsed as its own snippet; it must still see the
    // program's function signatures, so a call inside a hole resolves named arguments and
    // fills defaults exactly like the same call outside a string. (Regression: the hole used
    // to parse with an empty signature table, so `"{f(x, k: v)}"` errored.)
    let src = concat!(
        "fn f(x, k = 10) = x + k\n",
        "print(\"a={f(1)}\")\n",          // default fills → 11
        "print(\"b={f(1, k: 5)}\")\n",    // named arg → 6
        "y = 3.14159\n",
        "print(\"c={y:.2f}\")\n",         // format spec still works → 3.14
    );
    let (out, err, code) = run_source(src, &[], "interpnamed");
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(out.lines().collect::<Vec<_>>(), ["a=11", "b=6", "c=3.14"]);
}

#[test]
fn dict_serializes_to_a_json_object() {
    // A Dict is the natural carrier for a dynamic-keyed payload (arbitrary string keys
    // decided at runtime); it must serialize as a JSON object. Build one from pairs,
    // serialize, re-parse, and read a value back — the round trip proves the object form.
    let src = concat!(
        "d = [(\"model\", \"opus\"), (\"n\", 3)].to_dict()\n",
        "s = d.to_json()\n",
        "print(s)\n",
        "back = s.parse_json()\n",
        "print(back.model)\n", // opus
        "print(back.n)\n",     // 3
    );
    let (out, stderr, code) = run_source(src, &[], "dictjson");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let lines: Vec<&str> = out.lines().collect();
    // Dict keys are sorted (BTreeMap), so the object order is stable: model before n.
    assert_eq!(lines[0], "{\"model\":\"opus\",\"n\":3}");
    assert_eq!(lines[1], "opus");
    assert_eq!(lines[2], "3");
}

#[test]
fn try_catches_runtime_errors() {
    // `try EXPR` yields {ok, value, error}; a runtime error is caught (not aborting),
    // and recovery composes with `??`.
    let src = concat!(
        "ok = try (10 * 2)\n",
        "print(ok.ok)\n",                                   // true
        "print(ok.value)\n",                                // 20
        "bad = try [1, 2, 3][99]\n",
        "print(bad.ok)\n",                                  // false (out-of-bounds caught)
        "v = (try \"[1,\".parse_json()).value ?? \"fallback\"\n",
        "print(v)\n",                                       // fallback
        "print(\"continues\")\n",                           // program did not abort
    );
    let (out, stderr, code) = run_source(src, &[], "try");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "true\n20\nfalse\nfallback\ncontinues");
}

#[test]
fn record_string_indexing() {
    // `r["key"]` — dynamic field access; an absent key is `missing` (the optional
    // accessor). Useful for JSON whose keys aren't valid identifiers.
    let src = "r = {a: 1, b: 2}\nprint(r[\"a\"])\nprint(r[\"b\"])\nprint(r[\"z\"].is_missing())\n";
    let (out, stderr, code) = run_source(src, &[], "recidx");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "1\n2\ntrue");
}

#[test]
fn read_fastq_parses_reads_with_quality() {
    // FASTQ -> records {id, seq, qual, length}; sequence methods apply to `seq`.
    let src = "r = read_fastq(\"examples/data/reads.fastq\")\nprint(r.count())\nprint(r.first().length)\nprint(r.first().seq.gc_content())\n";
    let (out, stderr, code) = run_source(src, &[], "fastq");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n12\n0.5"); // 3 reads; first is 12 bp; GC = 0.5
}

#[test]
fn named_arguments_and_defaults() {
    // A user function can declare literal-constant defaults; calls may pass arguments
    // by name (in any order) and omit defaulted parameters. Resolved to positional
    // form at parse time, so both engines behave identically.
    let src = "fn greet(name, greeting = \"Hi\") = \"{greeting}, {name}\"\nprint(greet(\"Ada\"))\nprint(greet(\"Ada\", greeting: \"Hey\"))\nfn vol(w, h, d = 1) = w * h * d\nprint(vol(2, 3))\nprint(vol(2, d: 5, h: 3))\n";
    let (out, stderr, code) = run_source(src, &[], "named");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "Hi, Ada\nHey, Ada\n6\n30"); // default, named, default-fill, mixed
}

#[test]
fn named_argument_errors_are_clear() {
    for (src, want) in [
        ("fn f(a, b) = a\nf(1, c: 2)\n", "no parameter named `c`"),
        ("fn f(a, b) = a\nf(1, a: 2)\n", "given more than once"),
        ("fn f(a, b) = a\nf(a: 1)\n", "missing an argument for parameter `b`"),
        ("fn f(a, b = a) = a\nf(1)\n", "must be a literal constant"),
        ("print(range(start: 0))\n", "only supported for user-defined functions"),
    ] {
        let (_o, stderr, code) = run_source(src, &[], "namederr");
        assert_eq!(code, Some(1), "expected failure for `{src}`");
        assert!(stderr.contains(want), "src `{src}`\nwant `{want}`\nstderr:\n{stderr}");
    }
}

#[test]
fn align_pairwise_global_local_semiglobal() {
    // ADR 0015 hand-rolled affine-gap aligner. Global scores a single mismatch
    // (3=1X4=); local extracts a conserved core at +7; semiglobal fits a whole read
    // into a target window, reporting the [start, end) placement.
    let src = "g = dna(\"ACGTACGT\").align(dna(\"ACGAACGT\"))\nprint(g.score)\nprint(g.cigar)\nl = dna(\"TTGATTACATT\").align(dna(\"CCGATTACAGG\"), \"local\")\nprint(l.score)\nprint(l.cigar)\nprint(l.start)\nprint(l.end)\ns = dna(\"GATTACA\").align(dna(\"CCCGATTACAGGG\"), \"semiglobal\")\nprint(s.score)\nprint(s.start)\nprint(s.end)\n";
    let (out, stderr, code) = run_source(src, &[], "align");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3=1X4=\n7\n7=\n2\n9\n7\n3\n10");
}

#[test]
fn align_rejects_an_unknown_mode() {
    let src = "print(dna(\"AC\").align(dna(\"AC\"), \"fuzzy\").score)\n";
    let (_out, stderr, code) = run_source(src, &[], "alignmode");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("unknown alignment mode"), "stderr:\n{stderr}");
}

#[test]
fn huge_alignment_is_capped_not_oom() {
    // A 20000x20000 DP matrix (400M cells) would exhaust memory; the cap turns it into
    // a clean error instead of an OOM/abort. Runs on the big stack as a subprocess, so
    // the sequence-building recursion is fine.
    let src = "fn rep(n) = if n <= 0 then \"\" else \"ACGTACGTAC{rep(n - 1)}\"\nbig = dna(rep(2000))\nprint(big.align(big).score)\n";
    let (_out, stderr, code) = run_source(src, &[], "aligncap");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("too large"), "stderr:\n{stderr}");
}

#[test]
fn broken_pipe_does_not_panic() {
    // `helix … | head` closes stdout early. With SIGPIPE reset to its default the
    // process is terminated cleanly by the signal — it must NOT emit a Rust panic /
    // "Broken pipe" backtrace.
    use std::io::Read;
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["eval", "print((0..1000000).map(it))"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read a few bytes, then drop the read end (closing the pipe), like `head`.
    let mut buf = [0u8; 8];
    let _ = child.stdout.take().unwrap().read(&mut buf);
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "broken pipe panicked:\n{stderr}");
    assert!(!stderr.contains("Broken pipe"), "broken pipe surfaced:\n{stderr}");
}

#[test]
fn phred_decodes_quality_and_filters_reads() {
    // `qual.phred()` decodes a Phred+33 quality string to integer scores, which
    // compose with the array verbs (mean/min) for QC. The first read's quality is
    // all `I` (ASCII 73 -> Q40), so a quality-filter is one `where`. A read with no
    // quality line (a FASTA read through read_fastq) has `qual = missing`.
    let src = "r = read_fastq(\"examples/data/reads.fastq\")\nprint(r.first().qual.phred().mean())\nprint(\"IIH\".phred())\nprint(r.where(it.qual.phred().mean() >= 38).count())\nf = read_fastq(\"examples/data/sample.fa\").first()\nprint(f.qual.is_missing())\n";
    let (out, stderr, code) = run_source(src, &[], "phred");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // Q40 mean; [40,40,39]; all 3 reads pass mean>=38; FASTA-sourced qual is missing.
    assert_eq!(out.trim(), "40.0\n[40, 40, 39]\n3\ntrue");
}

#[test]
fn read_vcf_accepts_gzipped_files() {
    // Real-world VCFs are bgzipped `.vcf.gz`; the reader sniffs the gzip magic bytes
    // and decompresses transparently, so a `.vcf.gz` queries identically to its plain
    // form (the fixture is the gzip of examples/data/variants.vcf).
    let src = "v = read_vcf(\"examples/data/variants.vcf.gz\")\nprint(v.count())\nprint(v.where(gene == \"BRCA1\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcfgz");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3"); // identical to the plain-VCF result
}

#[test]
fn read_vcf_makes_variants_queryable() {
    // The bio flagship: a VCF becomes a DataFrame the normal verbs work on. INFO
    // fields (gene) are columns alongside the fixed ones (qual). No group-by here, so
    // counts are deterministic.
    // `af` is a header-typed Float INFO column, so `af > 0.001` is a NUMERIC
    // comparison (3 rows) — a plain string column would mis-compare and give 5.
    let src = "v = read_vcf(\"examples/data/variants.vcf\")\nprint(v.count())\nprint(v.where(gene == \"BRCA1\").count())\nprint(v.where(qual > 50).count())\nprint(v.where(af > 0.001).count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcf");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3\n3\n3"); // 6 variants; 3 BRCA1; 3 qual>50; 3 af>0.001
}

#[test]
fn read_bcf_queries_identically_to_read_vcf() {
    // BCF is the binary, BGZF-framed form of VCF. read_bcf shares read_vcf's record
    // model and column-building, so the same queries over the binary fixture must
    // give the SAME answers as the text VCF (including the header-typed Float `af`
    // column, so `af > 0.001` stays a numeric comparison). The fixture is generated
    // from variants.vcf by the ignored `generate_bcf_fixture` test in src/vcf.rs.
    let src = "b = read_bcf(\"examples/data/variants.bcf\")\nprint(b.count())\nprint(b.where(gene == \"BRCA1\").count())\nprint(b.where(qual > 50).count())\nprint(b.where(af > 0.001).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bcf");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3\n3\n3"); // identical to the plain-VCF result above
}

#[test]
fn read_vcf_region_query_uses_the_index() {
    // The local-first capability: `read_vcf(path, region)` seeks via the `.tbi` index
    // and returns only the variants intersecting the region, identical to a full read
    // filtered to that window (INFO columns preserved). The bgzipped+indexed fixture is
    // generated from variants.vcf by the ignored `generate_vcf_index_fixture` test.
    let src = "p = \"examples/data/variants.vcf.gz\"\nprint(read_vcf(p, \"chr17:43044000-43046000\").count())\nprint(read_vcf(p, \"chr13\").count())\nprint(read_vcf(p, \"chr17:43090000-43100000\").select(pos, gene).column(\"pos\").first())\nprint(read_vcf(p).count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcfregion");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // 2 in the chr17 window; 2 on chr13; the tail window's single variant is at 43091983;
    // a plain read of the same (now BGZF) file still scans all 6.
    assert_eq!(out.trim(), "2\n2\n43091983\n6");
}

#[test]
fn read_vcf_region_without_index_is_a_clean_error() {
    // A region query against a file with no `.tbi` (here the plain, unindexed .vcf)
    // fails with a clear message rather than a panic.
    let src = "print(read_vcf(\"examples/data/variants.vcf\", \"chr17:1-9999999\").count())\n";
    let (_out, stderr, code) = run_source(src, &[], "vcfnoidx");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("indexed") || stderr.contains(".tbi"), "stderr:\n{stderr}");
}

#[test]
fn read_sam_makes_alignments_queryable() {
    // The alignment flagship: a SAM file becomes a DataFrame with the eleven mandatory
    // fields as columns. `ref` is resolved from the header (null for an unmapped read),
    // `mapq` is a numeric column, and the CIGAR is rendered to its SAM string.
    let src = "a = read_sam(\"examples/data/alignments.sam\")\nprint(a.count())\nprint(a.where(ref == \"chr1\").count())\nprint(a.where(mapq > 50).count())\nprint(a.where(name == \"read2\").column(\"cigar\").first())\n";
    let (out, stderr, code) = run_source(src, &[], "sam");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n2\n2\n5M2I1M"); // 4 reads; 2 on chr1; 2 mapq>50; read2 CIGAR
}

#[test]
fn read_bam_queries_identically_to_read_sam() {
    // BAM is the binary, BGZF-framed form of SAM. read_bam shares read_sam's record
    // model and column-building, so the same queries over the binary fixture give the
    // SAME answers. The fixture is generated from alignments.sam by the ignored
    // `generate_bam_fixture` test in src/sam.rs.
    let src = "b = read_bam(\"examples/data/alignments.bam\")\nprint(b.count())\nprint(b.where(ref == \"chr1\").count())\nprint(b.where(mapq > 50).count())\nprint(b.where(name == \"read2\").column(\"cigar\").first())\n";
    let (out, stderr, code) = run_source(src, &[], "bam");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n2\n2\n5M2I1M"); // identical to the plain-SAM result above
}

#[test]
fn read_bam_region_query_uses_the_index() {
    // The local-first capability for alignments: `read_bam(path, region)` seeks via the
    // `.bai` index and returns only the reads intersecting the region (by CIGAR-spanned
    // reference coordinates), identical to a full read filtered to the window. The
    // indexed BAM+`.bai` fixture is generated by the ignored `generate_bam_fixture` test.
    let src = "p = \"examples/data/alignments.bam\"\nprint(read_bam(p, \"chr1\").count())\nprint(read_bam(p, \"chr2\").count())\nprint(read_bam(p, \"chr1:140-160\").count())\nprint(read_bam(p).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bamregion");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // 2 reads on chr1; 1 on chr2; 1 read (read2 @150) spans chr1:140-160; scan reads all 4.
    assert_eq!(out.trim(), "2\n1\n1\n4");
}

#[test]
fn read_gff_makes_features_queryable() {
    // A GFF3 file becomes a DataFrame: the standard feature columns plus one string
    // column per attribute tag (so `Name` is queryable alongside `type`/`strand`).
    let src = "g = read_gff(\"examples/data/genes.gff3\")\nprint(g.where(type == \"gene\").count())\nprint(g.where(Name == \"BRCA1\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "gff");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n1"); // 3 gene features; 1 named BRCA1
}

#[test]
fn read_bed_makes_intervals_queryable() {
    // A BED file becomes a DataFrame; the optional name/score/strand columns appear
    // because the file carries them, and `score` is numeric (`score > 400`).
    let src = "b = read_bed(\"examples/data/peaks.bed\")\nprint(b.count())\nprint(b.where(score > 400).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bed");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n3"); // 4 intervals; 3 with score > 400
}

#[test]
fn higher_order_functions_work_on_both_engines() {
    // A function-valued parameter is callable (`f(x)`): the gradual checker permits
    // an Unknown-typed name as a call target. Runtime already supported it — this
    // pins the checker fix and VM/tree-walker agreement through the real CLI.
    let src = "fn inc(n) = n + 1\nfn apply(f, x) = f(x)\nfn twice(f, x) = f(f(x))\nprint(apply(inc, 5))\nprint(apply((n => n * 2), 5))\nprint(twice(inc, 5))\n";
    let (vm, e1, c1) = run_source(src, &[], "hof_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "6\n10\n7"); // apply(inc,5); apply(double,5); twice(inc,5)
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "hof_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on higher-order functions");
}

#[test]
fn closures_capture_on_both_engines() {
    // Standalone closures that capture an enclosing local — returned, stored, called
    // later — work identically on the VM (upvalues) and the tree-walker (env capture),
    // including capturing function-valued params and two-level nesting.
    let src = concat!(
        "fn make(k) = (p => p + k)\n",
        "g = make(10)\n",
        "print(g(5))\n",
        "fn inc(n) = n + 1\n",
        "fn dbl(n) = n * 2\n",
        "fn compose(f, h) = (x => f(h(x)))\n",
        "comp = compose(inc, dbl)\n",
        "print(comp(10))\n",
        "fn outer(a) = (b => (cc => a + b + cc))\n",
        "p1 = outer(1)\n",
        "p2 = p1(2)\n",
        "print(p2(3))\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "clo_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "15\n21\n6"); // make+10 then +5; inc(dbl(10)); 1+2+3
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "clo_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on closures");
}

#[test]
fn match_works_on_both_engines() {
    // `match` with literal arms + wildcard, a binding pattern (and recursion through
    // arms), and the `missing` pattern — identical on the VM (compiled to test/jump
    // ops sharing the tree-walker's matcher) and the tree-walker.
    let src = concat!(
        "print(match 2 { 1 => \"one\", 2 => \"two\", _ => \"other\" })\n",
        "fn fib(n) = match n { 0 => 0, 1 => 1, _ => fib(n - 1) + fib(n - 2) }\n",
        "print(fib(10))\n",
        "print(match missing { missing => \"absent\", _ => \"present\" })\n",
        "print(match 42 { x => x + 1 })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "match_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "two\n55\nabsent\n43");
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "match_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on `match`");
}

#[test]
fn match_nested_patterns_on_both_engines() {
    // Tuple + record patterns (with a partial match), and the killer case:
    // destructuring a `try` result. Identical on both engines.
    let src = concat!(
        "print(match (1, 2) { (a, b) => a + b })\n",
        "print(match {a: 1, b: 2} { {b: x} => x, _ => 0 })\n",
        "fn unwrap(r) = match r { {ok: true, value: v} => v, _ => -1 }\n",
        "print(unwrap(try (20 / 4)))\n",
        "print(unwrap(try (1 / 0)))\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchn_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "3\n2\n5.0\n-1"); // tuple sum; record field; try ok; try err
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchn_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on nested patterns");
}

#[test]
fn match_or_patterns_on_both_engines() {
    // `a | b | c` matches if any alternative does; composes inside a tuple pattern
    // (with a sibling binding). Identical on both engines.
    let src = concat!(
        "print(match 2 { 1 | 2 | 3 => \"low\", _ => \"high\" })\n",
        "print(match 9 { 1 | 2 | 3 => \"low\", _ => \"high\" })\n",
        "print(match (1, 5) { (1 | 2, x) => x, _ => 0 })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchor_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "low\nhigh\n5"); // in-set; not-in-set; or inside a tuple
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchor_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on or-patterns");
}

#[test]
fn match_guards_on_both_engines() {
    // `pat if cond => ...` — an arm is taken only if the guard (with the pattern's
    // bindings in scope) holds, else the next arm is tried. Identical on both engines.
    let src = concat!(
        "print(match 5 { n if n > 3 => \"big\", _ => \"small\" })\n",
        "print(match 2 { n if n > 3 => \"big\", _ => \"small\" })\n",
        "print(match (1, 2) { (a, b) if a < b => \"asc\", _ => \"other\" })\n",
        "print(match try (10 / 2) { {ok: true, value: v} if v > 3 => \"big\", _ => \"other\" })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchg_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "big\nsmall\nasc\nbig"); // guard true; false; tuple-bind guard; try+guard
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchg_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on match guards");
}

#[test]
fn with_derives_columns_from_expressions() {
    // `df.with({name: expr, ...})` adds columns computed over existing ones. The
    // value expressions reference bare column names, like the other column verbs.
    let src = "v = read_vcf(\"examples/data/variants.vcf\")\nd = v.with({strong: qual > 50})\nprint(d.where(strong).count())\n";
    let (out, stderr, code) = run_source(src, &[], "with");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3"); // 3 of 6 variants have qual > 50
}

#[test]
fn doc_lists_methods_by_type() {
    // `helix doc` overview names every receiver type.
    let (out, _, code) = run(&["doc"], &[], "");
    assert_eq!(code, Some(0));
    assert!(out.contains("Array") && out.contains("Dna") && out.contains("DataFrame"));
    // `helix doc <Type>` lists that type's methods, incl. recently-added ones.
    let (dna, _, c2) = run(&["doc", "Dna"], &[], "");
    assert_eq!(c2, Some(0));
    assert!(dna.contains("base_counts") && dna.contains("hamming") && dna.contains("gc_content"));
    // case-insensitive type name.
    let (arr, _, _) = run(&["doc", "array"], &[], "");
    assert!(arr.contains("scan") && arr.contains("take_while") && arr.contains("index_of"));
    // free functions.
    let (b, _, _) = run(&["doc", "builtins"], &[], "");
    assert!(b.contains("sqrt") && b.contains("read_csv"));
    // an unknown type is a clear error, not a panic.
    let (_, err, c3) = run(&["doc", "Nope"], &[], "");
    assert_eq!(c3, Some(1));
    assert!(err.contains("unknown type"));
}

#[test]
fn join_combines_frames_on_a_key() {
    // `a.join(b, key)` defaults to an inner join; a trailing string picks the type.
    // samples has S1..S4; sample_meta has S1..S3, S5 — so inner keeps 3, left keeps 4.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nm = read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, sample_id).count())\nprint(s.join(m, sample_id, \"left\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "join");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n4");
}

#[test]
fn import_resolves_on_the_search_path() {
    // A module that is not beside the script resolves via `HELIX_PATH` — the mechanism
    // shared user libraries (and a future stdlib) rely on.
    let lib = std::env::temp_dir().join("helix_sp_lib");
    let _ = std::fs::remove_dir_all(&lib);
    std::fs::create_dir_all(lib.join("tools")).unwrap();
    std::fs::write(lib.join("tools").join("util.helix"), "export fn triple(x) = x * 3\n").unwrap();
    let src = "import tools.util as u\nprint(u.triple(7))\n";
    let (out, stderr, code) =
        run_source(src, &[("HELIX_PATH", lib.to_str().unwrap())], "searchpath");
    let _ = std::fs::remove_dir_all(&lib);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "21");
}

#[test]
fn bio_sequence_helpers_over_fastq() {
    // The native sequence-array helpers over the reads of a FASTQ file.
    let src = "r = read_fastq(\"examples/data/reads.fastq\")\nseqs = r.map(x => x.seq)\nprint(seqs.total_length())\nprint(seqs.mean_gc() > 0.4)\n";
    let (out, stderr, code) = run_source(src, &[], "bioseq");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "36\ntrue"); // 3 reads x 12 bp; mean GC ~0.44
}

#[test]
fn selective_import_binds_names_unqualified() {
    // `import m.{a, b}` brings the chosen names into scope without the namespace.
    let lib = "export fn triple(x) = x * 3\nexport fn quad(x) = x * 4\n";
    let main = "import lib.{triple, quad}\nprint(triple(5))\nprint(quad(2))\n";
    let (out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "selimp");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "15\n8");
}

#[test]
fn private_module_members_are_not_reachable() {
    // Only `export`ed names cross a module boundary (ADR 0019). A private name is a hard
    // boundary — qualified access fails at the use site, naming the module.
    let lib = "export fn pub_fn(x) = x + 1\n_secret = 42\n";
    let main = "import lib\nprint(lib._secret)\n";
    let (_, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "priv");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("not exported by module `lib`"), "stderr:\n{stderr}");
    // The exported one works fine.
    let ok = "import lib\nprint(lib.pub_fn(4))\n";
    let (out, _, code2) =
        run_modules(&[("lib.helix", lib), ("main.helix", ok)], "main.helix", &[], "priv_ok");
    assert_eq!(code2, Some(0));
    assert_eq!(out.trim(), "5");
}

#[test]
fn imported_module_may_not_run_side_effects() {
    // A module is definitions-only: a bare top-level expression (a stray `print`) is an
    // error, so importing never executes arbitrary code. The error points into the
    // library file, not the entry.
    let lib = "export fn f(x) = x + 1\nprint(\"side effect\")\n";
    let main = "import lib\nprint(lib.f(1))\n";
    let (_, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "deffx");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("may only contain definitions"), "stderr:\n{stderr}");
    assert!(stderr.contains("lib.helix:2"), "should point into the library:\n{stderr}");
}

#[test]
fn selective_import_of_a_private_name_fails_at_the_import() {
    // `import m.{x}` where `x` isn't exported errors at the import, naming the module —
    // not later at the use site.
    let lib = "export fn a(x) = x\nfn b(x) = x\n";
    let main = "import lib.{b}\nprint(b(1))\n";
    let (_, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "selpriv");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("`b` is not exported by module `lib`"), "stderr:\n{stderr}");
    assert!(stderr.contains("main.helix:1"), "should point at the import line:\n{stderr}");
}

#[test]
fn export_is_a_contextual_keyword() {
    // `export` is special only before a definition; elsewhere it's an ordinary name, so
    // existing code that used `export` as an identifier keeps working.
    let (out, stderr, code) = run(&["eval", "export = 5\nprint(export + 1)"], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6");
}

#[test]
fn imports_resolve_from_the_project_root() {
    // A file in a subdirectory can import a module elsewhere in the project by its
    // root-relative path — not just files sitting beside it. The root is the helix.toml
    // directory (and, with no manifest, the entry file's own directory).
    let dir = std::env::temp_dir().join("helix_rootimp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("utils.helix"), "export fn double(x) = x * 2\n").unwrap();
    // `sub/needs.helix` imports the ROOT module `utils`, which is not beside it.
    std::fs::write(
        dir.join("sub/needs.helix"),
        "import utils\nexport fn sext(x) = utils.double(x) * 3\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.helix"), "import sub.needs\nprint(needs.sext(2))\n").unwrap();
    let entry = dir.join("main.helix");
    let entry = entry.to_str().unwrap();

    // With a manifest (root = the helix.toml directory).
    std::fs::write(dir.join("helix.toml"), "[package]\nname = \"app\"\n").unwrap();
    let (out, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "with manifest; stderr:\n{stderr}");
    assert_eq!(out.trim(), "12");

    // Without a manifest (root = the entry file's directory). Still resolves.
    std::fs::remove_file(dir.join("helix.toml")).unwrap();
    let (out, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "without manifest; stderr:\n{stderr}");
    assert_eq!(out.trim(), "12");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_map_filter_kernels_agree_across_all_paths() {
    // `map`/`filter` over an Int array compile to a native JIT kernel on the VM. The
    // result must be byte-identical across the VM native kernel (default), the
    // tree-walker oracle (HELIX_NOVM), and the bytecode loop (HELIX_NOJIT).
    let src = "xs = range(0, 1000)\n\
               m = xs.map(x => x * x - 3 * x + 1)\n\
               f = xs.filter(x => x % 7 == 0)\n\
               g = xs.map(x => if x > 500 then x * 2 else 0 - x)\n\
               print(m.sum())\nprint(f.count())\nprint(f.sum())\nprint(g.sum())\n";
    let (vm, stderr, code) = run_source(src, &[], "kern_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], "kern_tw");
    let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], "kern_nojit");
    assert_eq!(vm, tw, "native kernel vs tree-walker oracle");
    assert_eq!(vm, nojit, "native kernel vs bytecode loop");
    assert!(!vm.trim().is_empty());
}

#[test]
fn fused_pipelines_match_the_oracle() {
    // A chain of map/filter (± a reduce sink) over an Int source compiles to ONE native
    // loop with no intermediate arrays. The fused result must be byte-identical to the
    // tree-walker (which materializes every stage) and the bytecode loop.
    let cases = [
        // filter→map (array, Collect)
        "print([1,2,3,4,5,6,7,8,9,10].filter(x => x % 2 == 0).map(x => x * x))",
        // map→filter→map (3 stages, Collect)
        "print([1,2,3,4,5,6,7,8].map(x => x + 1).filter(x => x > 4).map(x => x * 10))",
        // range→map→filter→reduce (the zero-allocation scalar pipeline)
        "print(range(0, 200).map(x => x * x).filter(x => x % 3 == 0).reduce(0, (a, x) => a + x))",
        // array→map→reduce (1 stage + reduce)
        "print([1,2,3,4,5].map(x => x * 2).reduce(0, (a, x) => a + x))",
        // range→filter→map→reduce
        "print(range(1, 500).filter(x => x % 7 == 0).map(x => x - 1).reduce(0, (a, x) => a + x))",
        // filter→count (the zero-allocation counting sink)
        "print([1,2,3,4,5,6,7,8,9,10].filter(x => x % 2 == 0).count())",
        // range→map→filter→count
        "print(range(0, 100).map(x => x * x).filter(x => x % 3 == 0).count())",
    ];
    for (i, src) in cases.iter().enumerate() {
        let (vm, stderr, code) = run_source(src, &[], &format!("fuse_vm{i}"));
        assert_eq!(code, Some(0), "case {i} stderr:\n{stderr}");
        let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], &format!("fuse_tw{i}"));
        let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], &format!("fuse_nj{i}"));
        assert_eq!(vm, tw, "case {i}: fused vs tree-walker:\n{src}");
        assert_eq!(vm, nojit, "case {i}: fused vs bytecode:\n{src}");
    }
}

#[test]
fn kernel_bodies_can_call_helper_functions() {
    // A kernel/fused body may call JIT-eligible user functions — the function is compiled
    // natively and called from inside the loop. Must match the oracle on every path.
    let cases = [
        "fn sq(x) = x * x\nprint([1,2,3,4,5].map(x => sq(x)))",
        "fn g(x) = x * 3\nfn f(x) = g(x) + 1\nprint([1,2,3,4].map(x => f(x)).filter(x => x % 2 == 0))",
        "fn sq(x) = x * x\nprint([1,2,3,4,5,6].filter(x => sq(x) > 9))",
        "fn dbl(x) = x * 2\nprint(range(0, 50).map(x => dbl(x)).filter(x => x > 30).reduce(0, (a, x) => a + x))",
    ];
    for (i, src) in cases.iter().enumerate() {
        let (vm, stderr, code) = run_source(src, &[], &format!("fnk_vm{i}"));
        assert_eq!(code, Some(0), "case {i} stderr:\n{stderr}");
        let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], &format!("fnk_tw{i}"));
        let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], &format!("fnk_nj{i}"));
        assert_eq!(vm, tw, "case {i}: native (fn-call) vs tree-walker:\n{src}");
        assert_eq!(vm, nojit, "case {i}: native (fn-call) vs bytecode:\n{src}");
    }
}

#[test]
fn ineligible_map_bodies_fall_through_correctly() {
    // A float array (no Int kernel) and a non-arithmetic body both bypass the kernel
    // and run the bytecode loop — still correct, and identical to the tree-walker.
    let src = "print([1.0, 4.0, 9.0].map(x => x * 2.0).sum())\n\
               print([1,2,3].map(x => sqrt(x * 1.0)).count())\n";
    let (vm, stderr, code) = run_source(src, &[], "fall_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], "fall_tw");
    assert_eq!(vm, tw);
    assert_eq!(vm.trim(), "28.0\n3");
}

#[test]
fn assertions_raise_with_a_message_and_are_catchable() {
    // A passing assert is silent; a failing one raises a clean, catchable error.
    let (out, stderr, code) = run_source(
        "assert(1 < 2)\nr = try assert(false, \"nope\")\nprint(r.ok)\nprint(r.error)\n",
        &[],
        "assertok",
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "false\nassertion failed: nope");

    // An uncaught failure exits non-zero with the message.
    let (_o, stderr, code) = run_source("assert_eq(1, 2)\n", &[], "assertfail");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("assertion failed: 1 != 2"), "stderr:\n{stderr}");
}

#[test]
fn helix_test_runs_test_files_and_reports() {
    // `helix test` discovers `*_test.helix` files, runs each in isolation, and exits
    // non-zero iff any failed. A test passes by running to completion without raising;
    // `assert*` raise on failure.
    let dir = std::env::temp_dir().join("helix_testrun");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("math.helix"), "export fn double(x) = x * 2\n").unwrap();
    // Passing: imports a project module (root-anchored), asserts.
    std::fs::write(
        dir.join("math_test.helix"),
        "import math\nfn test_double() = assert_eq(math.double(3), 6)\ntest_double()\n",
    )
    .unwrap();
    // Passing nested test (float closeness).
    std::fs::write(
        dir.join("sub/calc_test.helix"),
        "fn test_close() = assert_close(0.1 + 0.2, 0.3)\ntest_close()\n",
    )
    .unwrap();
    // A non-test file must be ignored.
    std::fs::write(dir.join("helper.helix"), "print(\"should not run\")\n").unwrap();

    // All pass → exit 0, summary present.
    let (out, stderr, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}\nout:\n{out}");
    assert!(out.contains("2 passed"), "out:\n{out}");
    assert!(!out.contains("should not run"), "ran a non-test file:\n{out}");

    // Add a failing test → exit 1, the failure and its assertion message are reported.
    std::fs::write(
        dir.join("broken_test.helix"),
        "fn test_bad() = assert_eq(2 + 2, 5)\ntest_bad()\n",
    )
    .unwrap();
    let (out, _stderr, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "a failing test must exit non-zero:\n{out}");
    assert!(out.contains("FAIL"), "out:\n{out}");
    assert!(out.contains("4 != 5"), "out:\n{out}");
    assert!(out.contains("2 passed, 1 failed"), "out:\n{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_module_on_search_path_is_a_clean_error() {
    // An import found neither locally nor on the search path fails with a clear message.
    let src = "import nowhere.lib as x\nprint(1)\n";
    let (_out, stderr, code) = run_source(src, &[], "missmod");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot find module `nowhere.lib`"), "stderr:\n{stderr}");
}

#[test]
fn descriptive_statistics_and_correlation() {
    // Population statistics (so var == std^2) plus Pearson correlation, with the
    // missing-propagation rule: a `missing` in either series yields `missing`.
    let src = "xs = [2, 4, 4, 4, 5, 5, 7, 9]\nprint(xs.median())\nprint(xs.var())\nprint(xs.std())\nprint(correlation([1, 2, 3, 4], [2, 4, 6, 8]))\nprint(correlation([1, 2, 3], [1, missing, 3]))\n";
    let (out, stderr, code) = run_source(src, &[], "stats");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4.5\n4.0\n2.0\n1.0\nmissing");
}

#[test]
fn inferential_statistics_t_test_and_normal() {
    // The normal CDF (broadcasting math) and Welch's two-sample t-test. The t-test
    // returns a {statistic, df, p_value} record whose fields are reachable.
    let src = "print(normal_cdf(0.0))\ncontrol = [5.1, 4.9, 5.0, 5.2, 4.8, 5.0]\ntreated = [5.6, 5.8, 5.5, 5.9, 5.7, 5.4]\nr = t_test(control, treated)\nprint(r.p_value < 0.01)\nprint(r.statistic < 0.0)\n";
    let (out, stderr, code) = run_source(src, &[], "ttest");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.5\ntrue\ntrue"); // strong, significant difference
}

#[test]
fn t_test_on_constant_samples_is_a_clean_error() {
    let src = "print(t_test([2, 2, 2], [2, 2, 2]))\n";
    let (_out, stderr, code) = run_source(src, &[], "ttesterr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("t-test is undefined"), "stderr:\n{stderr}");
}

#[test]
fn linear_regression_fits_and_predicts() {
    // OLS fit of a textbook dataset (R: intercept 2.2, slope 0.6, R^2 0.6), with
    // predictions recovered by broadcasting `slope * x + intercept`.
    let src = "x = [1.0, 2.0, 3.0, 4.0, 5.0]\ny = [2.0, 4.0, 5.0, 4.0, 5.0]\nf = linear_regression(x, y)\nprint(f.slope)\nprint(f.intercept)\nprint(f.r_squared)\nprint(f.slope * 6.0 + f.intercept)\n";
    let (out, stderr, code) = run_source(src, &[], "lm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.6\n2.2\n0.6\n5.8"); // predicted y at x = 6
}

#[test]
fn linear_regression_without_variance_is_a_clean_error() {
    let src = "print(linear_regression([1, 1, 1], [1, 2, 3]))\n";
    let (_out, stderr, code) = run_source(src, &[], "lmerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("linear regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn multiple_regression_recovers_coefficients() {
    // y = 1 + 2*x1 + 3*x2 exactly → coefficients [1, 2, 3], R^2 = 1. The result's
    // coefficients/p_values are parameter-indexed arrays (index 0 is the intercept).
    let src = "x1 = [1.0, 2.0, 3.0, 4.0, 5.0]\nx2 = [2.0, 1.0, 4.0, 3.0, 5.0]\ny = [9.0, 8.0, 19.0, 18.0, 26.0]\nf = multiple_regression([x1, x2], y)\nc = f.coefficients\nprint(c.count())\nprint(f.r_squared)\nprint(round(c[0]) == 1 and round(c[1]) == 2 and round(c[2]) == 3)\n";
    let (out, stderr, code) = run_source(src, &[], "mlr");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n1.0\ntrue"); // 3 coefficients; perfect fit; b = [1, 2, 3]
}

#[test]
fn multiple_regression_on_collinear_predictors_is_a_clean_error() {
    let src = "print(multiple_regression([[1, 2, 3, 4], [2, 4, 6, 8]], [1, 3, 2, 5]))\n";
    let (_out, stderr, code) = run_source(src, &[], "mlrerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("multiple regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn column_extracts_values_for_statistics() {
    // `df.column(name)` materializes a column as an array, so the array statistics
    // apply directly to loaded data. Polars nulls become `missing`, so `drop_missing`
    // composes before an aggregation.
    let src = "p = read_csv(\"examples/data/patients.csv\")\nprint(p.column(\"age\").median())\nv = read_vcf(\"examples/data/variants.vcf\")\nprint(v.column(\"qual\").drop_missing().count())\n";
    let (out, stderr, code) = run_source(src, &[], "dfcolumn");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "43.0\n6"); // median of 8 ages; 6 non-null quals
}

#[test]
fn column_with_unknown_name_is_a_clean_error() {
    let src = "print(read_csv(\"examples/data/patients.csv\").column(\"nope\"))\n";
    let (_out, stderr, code) = run_source(src, &[], "dfcolerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("no column `nope`"), "stderr:\n{stderr}");
}

#[test]
fn correlation_on_mismatched_lengths_is_a_clean_error() {
    let src = "print(correlation([1, 2], [1, 2, 3]))\n";
    let (_out, stderr, code) = run_source(src, &[], "corrlen");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("equal-length arrays"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn join_without_an_operand_is_a_clean_error() {
    // A no-argument `join` type-checks (DataFrame args are the unchecked runtime
    // boundary), so the compiler must stay total and emit the friendly diagnostic
    // rather than the "internal error ... please report" totality breach.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nprint(s.join())\n";
    let (out, stderr, code) = run_source(src, &[], "joinerr");
    assert_ne!(code, Some(0), "stdout:\n{out}");
    assert!(
        stderr.contains("`join` needs a DataFrame to join with"),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains("internal error"), "stderr:\n{stderr}");
}

#[test]
fn join_on_an_unknown_key_is_a_clean_error() {
    // Keys are validated against both schemas up front, so a typo reads as a Helix
    // error naming the frame and listing valid columns — not Polars' lazy-plan dump.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nm = read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, no_such_key).count())\n";
    let (out, stderr, code) = run_source(src, &[], "joinkey");
    assert_ne!(code, Some(0), "stdout:\n{out}");
    assert!(
        stderr.contains("no column `no_such_key` in the left frame"),
        "stderr:\n{stderr}"
    );
}

// Real network fetch — ignored by default so the suite stays offline-friendly.
// Run with: `cargo test -- --ignored`.
#[cfg(feature = "http")]
#[test]
#[ignore]
fn http_get_returns_a_status() {
    let src = "r = http_get(\"https://example.com\")\nprint(r.status)\n";
    let (out, stderr, code) = run_source(src, &[], "http");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "200");
}

// --- Python interop (Phase 6) ------------------------------------------------
// These run against whichever feature set the suite was built with: the
// feature-gated tests need `cargo test --features python` (and a Python
// interpreter on the box); the default build instead asserts the friendly
// "rebuild with --features python" error.

fn run_script(src: &str, env: &[(&str, &str)], tag: &str) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("helix_py_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.helix");
    std::fs::write(&path, src).unwrap();
    let r = run(&[path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_dir_all(&dir);
    r
}

#[cfg(feature = "python")]
#[test]
fn python_import_math_on_both_engines() {
    // Both surface syntaxes: the statement form (sugar) and the expression form.
    let src = "import python.math as m\nprint(m.sqrt(16.0))\nmod = python.import(\"math\")\nprint(mod.gcd(12, 18))\n";
    let (vm, stderr, code) = run_script(src, &[], "math_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm.trim(), "4.0\n6", "got: {vm:?}");
    let (tw, _, _) = run_script(src, &[("HELIX_NOVM", "1")], "math_tw");
    assert_eq!(vm, tw, "VM and tree-walker disagree on Python interop");
}

#[cfg(feature = "python")]
#[test]
fn python_object_is_opaque_until_to_array() {
    // A Python list stays an opaque PyObject (NOT silently an Array) — but it now
    // PRINTS as its Python value; `to_array` is the explicit materialization to native.
    let src = "import python.builtins as b\nxs = b.list(b.range(0, 4))\nprint(xs)\nprint(to_array(xs).sum())\n";
    let (out, stderr, code) = run_script(src, &[], "opaque");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "[0, 1, 2, 3]\n6", "got: {out:?}");
}

#[cfg(feature = "python")]
#[test]
fn python_handles_forward_indexing_operators_and_eval() {
    // Indexing → __getitem__, operators → the operator protocol, and eval/exec share
    // a persistent namespace (the kwargs escape hatch). Verified on both engines.
    let src = "np = python.import(\"numpy\")\n\
               a = np.arange(5)\n\
               print(a[2])\n\
               print(a[1:4])\n\
               print(a[::-1])\n\
               print(a * 10)\n\
               print(a[1] < a[2])\n\
               python.exec(\"import numpy as N\")\n\
               print(python.eval(\"int(N.arange(4).sum())\"))\n\
               print(python.eval(\"2 ** 10\"))\n\
               print(python.import(\"json\").dumps({a: 1, b: 2}))\n";
    let (vm, stderr, code) = run_script(src, &[], "forward_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(
        vm.trim(),
        "2\n[1 2 3]\n[4 3 2 1 0]\n[ 0 10 20 30 40]\ntrue\n6\n1024\n{\"a\": 1, \"b\": 2}",
        "got: {vm:?}"
    );
    let (tw, _, _) = run_script(src, &[("HELIX_NOVM", "1")], "forward_tw");
    assert_eq!(vm, tw, "VM and tree-walker disagree on Python forwarding");
}

#[cfg(feature = "python")]
#[test]
fn python_exception_becomes_a_helix_error() {
    let src = "m = python.import(\"no_such_module_xyz\")\nprint(m)\n";
    let (_, stderr, code) = run_script(src, &[], "pymiss");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("python error: ModuleNotFoundError"),
        "stderr: {stderr:?}"
    );
}

#[cfg(feature = "python")]
#[test]
fn python_dataframe_round_trips_zero_copy() {
    // A Helix DataFrame flows out to Python's polars (len() = rows) and back via
    // `to_dataframe`, becoming a first-class Helix DataFrame again. Needs the Python
    // `polars` package; skip cleanly if it isn't installed so the suite stays portable.
    // The relative CSV path resolves because `run` sets cwd to the manifest dir.
    let src = concat!(
        "df = read_csv(\"examples/data/patients.csv\")\n",
        "print(python.import(\"builtins\").len(df))\n",
        "back = to_dataframe(python.import(\"polars\").concat([df]))\n",
        "print(back.count())\n",
    );
    let (out, stderr, code) = run_script(src, &[], "dfroundtrip");
    if stderr.contains("No module named 'polars'") {
        eprintln!("skipping python_dataframe_round_trips_zero_copy: Python polars not installed");
        return;
    }
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "8\n8", "got: {out:?}"); // 8 patients, preserved across the round-trip
}

#[cfg(feature = "python")]
#[test]
fn python_tensor_round_trips_via_numpy() {
    // A Helix Tensor crosses to NumPy and back via `to_tensor`, becoming a
    // first-class Helix Tensor again. Needs Python `numpy`; skip if it's absent.
    let src = concat!(
        "t = tensor([[1.0, 2.0], [3.0, 4.0]])\n",
        "np = python.import(\"numpy\")\n",
        "print(np.sum(t))\n",                  // to_py: Tensor -> NumPy -> scalar
        "print(to_tensor(np.transpose(t)).shape())\n", // round-trip -> native verb
    );
    let (out, stderr, code) = run_script(src, &[], "tensorroundtrip");
    if stderr.contains("No module named 'numpy'") {
        eprintln!("skipping python_tensor_round_trips_via_numpy: Python numpy not installed");
        return;
    }
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "10.0\n[2, 2]", "got: {out:?}");
}

#[cfg(not(feature = "python"))]
#[test]
fn python_without_feature_errors_with_rebuild_hint() {
    // The default build has the `python` global and parses `python.import`, but
    // calling it fails loudly with a build hint — never a cryptic runtime crash.
    let src = "m = python.import(\"math\")\nprint(m)\n";
    let (_, stderr, code) = run_script(src, &[], "nopy");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("without Python support"),
        "stderr: {stderr:?}"
    );
}

#[test]
fn capability_gate_audit_and_enforce() {
    // An `fs-read` builtin (resolves against the package root, where Cargo.toml exists).
    let src = "print(file_exists(\"Cargo.toml\"))\n";

    // Default (no HELIX_CAP): byte-identical to pre-capability Helix — no checks, no noise.
    let (out, err, code) = run_source(src, &[], "cap_default");
    assert_eq!(out.trim(), "true");
    assert!(!err.contains("capability"), "default run must not mention capabilities: {err:?}");
    assert_eq!(code, Some(0));

    // Audit: the fs-read is logged to stderr but still allowed (program succeeds).
    let (out, err, code) = run_source(src, &[("HELIX_CAP", "audit")], "cap_audit");
    assert_eq!(out.trim(), "true");
    assert!(err.contains("capability [audit] would deny fs-read"), "audit log missing: {err:?}");
    assert_eq!(code, Some(0));

    // Enforce without a grant: the fs-read is denied with a clear error (non-zero exit).
    let (_out, err, code) = run_source(src, &[("HELIX_CAP", "enforce")], "cap_enforce_deny");
    assert!(err.contains("capability denied"), "enforce should deny an ungranted fs-read: {err:?}");
    assert_ne!(code, Some(0));

    // Enforce WITH the matching grant: allowed again.
    let (out, _err, code) =
        run_source(src, &[("HELIX_CAP", "enforce"), ("HELIX_ALLOW_FS", "read")], "cap_enforce_grant");
    assert_eq!(out.trim(), "true");
    assert_eq!(code, Some(0));

    // A pure builtin is never gated, even under enforce.
    let (_out, err, code) = run_source("print(sqrt(16.0))\n", &[("HELIX_CAP", "enforce")], "cap_pure");
    assert!(!err.contains("capability denied"), "a pure builtin must not be gated: {err:?}");
    assert_eq!(code, Some(0));
}

#[test]
fn capability_gate_covers_write_methods() {
    // The `write_to` STRING METHOD is `fs-write` authority — a hole until phase 1b, since the
    // builtin gate only saw the `read_*` builtins. Writing to a path must be denied under
    // enforce without the grant, and allowed with it.
    let path = "/tmp/helix_cap_write_probe.txt";
    let _ = std::fs::remove_file(path);
    let src = format!("x = \"capdata\".write_to(\"{path}\")\nprint(\"wrote\")\n");

    // Enforce, no grant: the write is denied (fs-write), nothing is written.
    let (_out, err, code) = run_source(&src, &[("HELIX_CAP", "enforce")], "cap_w_deny");
    assert!(
        err.contains("capability denied") && err.contains("fs-write"),
        "enforce should deny an ungranted write_to: {err:?}"
    );
    assert_ne!(code, Some(0));
    assert!(!std::path::Path::new(path).exists(), "denied write must not touch the disk");

    // Enforce WITH the fs-write grant: allowed.
    let (out, _err, code) =
        run_source(&src, &[("HELIX_CAP", "enforce"), ("HELIX_ALLOW_FS", "write")], "cap_w_grant");
    assert_eq!(out.trim(), "wrote");
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_file(path);
}

#[test]
fn describe_emits_machine_readable_catalog() {
    // `helix describe` is the LLM/agent grounding surface: the whole API as JSON, each entry
    // tagged with its capability effect, sourced from the registry so it can't drift.
    let (out, _err, code) = run(&["describe"], &[], "");
    assert_eq!(code, Some(0));
    let v: serde_json::Value =
        serde_json::from_str(&out).expect("`helix describe` must emit valid JSON");

    // Builtins carry name + pure + effect. `read_text` is fs-read; `sqrt` is pure.
    let builtins = v["builtins"].as_array().expect("builtins array");
    let read_text = builtins.iter().find(|b| b["name"] == "read_text").expect("read_text listed");
    assert_eq!(read_text["effect"], "fs-read");
    assert_eq!(read_text["category"], "io");
    let sqrt = builtins.iter().find(|b| b["name"] == "sqrt").expect("sqrt listed");
    assert_eq!(sqrt["effect"], "pure");
    assert_eq!(sqrt["category"], "math");

    // Methods are grouped by receiver type, each tagged with its effect.
    let methods = v["methods"].as_object().expect("methods object");
    let has_map = methods
        .values()
        .any(|ms| ms.as_array().unwrap().iter().any(|m| m["name"] == "map"));
    assert!(has_map, "the `map` method should be in the catalog");
    let write_is_gated = methods
        .values()
        .any(|ms| ms.as_array().unwrap().iter().any(|m| m["name"] == "write_to" && m["effect"] == "fs-write"));
    assert!(write_is_gated, "`write_to` should be tagged fs-write");

    assert!(v["helix_version"].is_string(), "version present");
    assert!(v["universal_methods"].as_array().is_some(), "universal methods present");
    // The just-added client verb is discoverable and correctly tagged net.
    let http_post = v["builtins"].as_array().unwrap().iter().find(|b| b["name"] == "http_post");
    assert_eq!(http_post.expect("http_post listed")["effect"], "net");
}

#[test]
fn http_post_round_trips_to_a_helix_server() {
    use std::time::Duration;
    let dir = std::env::temp_dir();
    // Echo server (handles up to 100 requests, then exits): reply with method + body, so the
    // round trip proves the client sent a real POST with the right body.
    let srv = dir.join("helix_post_srv.helix");
    std::fs::write(
        &srv,
        "l = listen(18251)\n\
         served = range(0, 100).map(i => do {\n\
           c = l.accept()\n\
           r = c.request()\n\
           c.respond({ status: 201, json: { method: r.method, echo: r.body } })\n\
           0\n\
         })\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&srv)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");

    let cli = dir.join("helix_post_cli.helix");
    std::fs::write(
        &cli,
        "r = http_post(\"http://127.0.0.1:18251/\", \"hello-post\")\nprint(r.status)\nprint(r.body)\n",
    )
    .unwrap();

    // Retry the client until the listener is up (each attempt is a real POST; early ones
    // fail with a transport error until the server binds).
    let mut got = String::new();
    let mut ok = false;
    for _ in 0..40 {
        let (out, _e, code) = run(&[cli.to_str().unwrap()], &[], "");
        if code == Some(0) && out.contains("201") {
            got = out;
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&srv);
    let _ = std::fs::remove_file(&cli);

    assert!(ok, "http_post never succeeded against the server; last: {got:?}");
    assert!(got.contains("POST"), "server should report method POST: {got}");
    assert!(got.contains("hello-post"), "the POST body should be echoed: {got}");
}

#[test]
fn http_request_general_method_headers_and_response_headers() {
    use std::time::Duration;
    let dir = std::env::temp_dir();
    // Echo server: reflect the method, body, and a custom request header back.
    let srv = dir.join("helix_req_srv.helix");
    std::fs::write(
        &srv,
        "l = listen(18252)\n\
         served = range(0, 100).map(i => do {\n\
           c = l.accept()\n\
           r = c.request()\n\
           c.respond({ status: 200, json: { method: r.method, body: r.body, xtest: r.headers[\"x-test\"] } })\n\
           0\n\
         })\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&srv)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");

    // A PUT with a custom request header; assert the response carries headers back too.
    let cli = dir.join("helix_req_cli.helix");
    std::fs::write(
        &cli,
        "resp = http_request({ method: \"PUT\", url: \"http://127.0.0.1:18252/\", body: \"req-body\", headers: [[\"X-Test\", \"hi\"]] })\n\
         print(resp.status)\n\
         print(resp.body)\n\
         print(resp.headers.count() > 0)\n",
    )
    .unwrap();

    let mut got = String::new();
    let mut ok = false;
    for _ in 0..40 {
        let (out, _e, code) = run(&[cli.to_str().unwrap()], &[], "");
        if code == Some(0) && out.contains("200") {
            got = out;
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&srv);
    let _ = std::fs::remove_file(&cli);

    assert!(ok, "http_request never succeeded; last: {got:?}");
    assert!(got.contains("PUT"), "method PUT should reach the server: {got}");
    assert!(got.contains("req-body"), "the body should reach the server: {got}");
    assert!(got.contains("hi"), "the custom request header should reach the server: {got}");
    assert!(got.contains("true"), "response headers should be returned (count > 0): {got}");
}

#[test]
fn http_stream_pulls_chunks_line_by_line() {
    use std::time::Duration;
    let dir = std::env::temp_dir();
    // Server returns a 3-line body; the client pulls it chunk-by-chunk via `.next()`.
    let srv = dir.join("helix_stream_srv.helix");
    std::fs::write(
        &srv,
        "l = listen(18255)\n\
         served = range(0, 100).map(i => do {\n\
           c = l.accept()\n\
           x = c.request()\n\
           c.respond({ status: 200, text: \"chunk-a\\nchunk-b\\nchunk-c\" })\n\
           0\n\
         })\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&srv)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");

    // Pull-based client: status(), then next() until missing (EOF).
    let cli = dir.join("helix_stream_cli.helix");
    std::fs::write(
        &cli,
        "s = http_stream({ method: \"GET\", url: \"http://127.0.0.1:18255/\" })\n\
         print(s.status())\n\
         print(s.next())\n\
         print(s.next())\n\
         print(s.next())\n\
         print(s.next().is_missing())\n",
    )
    .unwrap();

    let mut got = String::new();
    let mut ok = false;
    for _ in 0..40 {
        let (out, _e, code) = run(&[cli.to_str().unwrap()], &[], "");
        if code == Some(0) && out.contains("200") {
            got = out;
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&srv);
    let _ = std::fs::remove_file(&cli);

    assert!(ok, "http_stream never succeeded; last: {got:?}");
    assert!(got.contains("chunk-a"), "first chunk: {got}");
    assert!(got.contains("chunk-b"), "second chunk: {got}");
    assert!(got.contains("chunk-c"), "third chunk: {got}");
    assert!(got.contains("true"), "the 4th next() should be missing at EOF: {got}");
}

#[cfg(feature = "http")]
#[test]
fn http_stream_close_cancels_early() {
    use std::time::Duration;
    let dir = std::env::temp_dir();
    // Server returns a 5-line body; the client reads one chunk, then `.close()` — the
    // early-cancel path (seen enough). A subsequent `.next()` must be `missing`, exactly
    // as at EOF, and `close()` again is a harmless no-op.
    let srv = dir.join("helix_stream_close_srv.helix");
    std::fs::write(
        &srv,
        "l = listen(18256)\n\
         served = range(0, 100).map(i => do {\n\
           c = l.accept()\n\
           x = c.request()\n\
           c.respond({ status: 200, text: \"a\\nb\\nc\\nd\\ne\" })\n\
           0\n\
         })\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_helix"))
        .arg(&srv)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");

    let cli = dir.join("helix_stream_close_cli.helix");
    std::fs::write(
        &cli,
        "s = http_stream({ method: \"GET\", url: \"http://127.0.0.1:18256/\" })\n\
         print(s.status())\n\
         print(s.next())\n\
         print(s.close().is_missing())\n\
         print(s.next().is_missing())\n\
         print(s.close().is_missing())\n",
    )
    .unwrap();

    let mut got = String::new();
    let mut ok = false;
    for _ in 0..40 {
        let (out, _e, code) = run(&[cli.to_str().unwrap()], &[], "");
        if code == Some(0) && out.contains("200") {
            got = out;
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&srv);
    let _ = std::fs::remove_file(&cli);

    assert!(ok, "http_stream never succeeded; last: {got:?}");
    assert!(got.contains("a\n"), "should read the first chunk before closing: {got}");
    assert!(!got.contains('b'), "must NOT read past close: {got}");
    // status, first chunk, then three `true`s: close→missing, next→missing, close→missing.
    assert_eq!(got.matches("true").count(), 3, "close/next after close all missing: {got}");
}

#[test]
fn record_dynamic_field_access() {
    // Dynamic access for unknown-shape data (a parsed JSON response): get/has/keys probe
    // fields by name at runtime, so a maybe-absent field is missing/false — not a compile
    // error. `get(k, default)` supplies a fallback.
    let src = "r = { name: \"Ada\", age: 36 }\n\
               print(r.get(\"name\"))\n\
               print(r.get(\"missing\") ?? \"none\")\n\
               print(r.get(\"missing\", \"def\"))\n\
               print(r.has(\"age\"))\n\
               print(r.has(\"nope\"))\n\
               print(r.keys().sort())\n";
    let (out, err, code) = run_source(src, &[], "rec_access");
    assert_eq!(code, Some(0), "err: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        vec!["Ada", "none", "def", "true", "false", "[\"age\", \"name\"]"],
        "record dynamic access: {out}"
    );
}

// ---------------------------------------------------------------------------
// Full-breadth sweep regressions (2026-07): closure capture shadowing, Unknown-
// receiver DataFrame dispatch, fn/global name collisions, parser depth guard.
// Each pins a verified tri-engine divergence or crash found by the sweep.
// ---------------------------------------------------------------------------

/// A nested lambda must capture the INNERMOST shadowed binding. The bytecode
/// builder kept duplicate names outer-first in `capturable_env` while
/// `resolve_upvalue` takes the first match, so VM/JIT captured the OUTERMOST
/// binding (`(f(10))(0)` = 10) where the walker read the innermost (11).
#[test]
fn closure_captures_innermost_shadowed_binding() {
    let src = "fn f(x) = let x = x + 1 in (y => x + y)\n\
print((f(10))(0))\n\
fn g(x) = match x + 1 { x => (z => x + z) }\n\
print((g(10))(0))\n\
fn h(x) = let y = 1 in let y = 2 in (z => y + z)\n\
print((h(0))(0))\n\
fn k(a) = let x = 1, x = 2 in (y => x + y)\n\
print((k(0))(0))\n\
fn m() = let x = 1, g = (u => x + u), x = 2, h = (u => x + u) in g(0) * 10 + h(0)\n\
print(m())\n";
    let mut outs = Vec::new();
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "cap_innermost");
        assert_eq!(code, Some(0), "stderr: {err}");
        outs.push(out);
    }
    assert_eq!(outs[0], outs[1], "JIT vs VM");
    assert_eq!(outs[1], outs[2], "VM vs tree-walker");
    let lines: Vec<&str> = outs[0].lines().collect();
    assert_eq!(lines, vec!["11", "11", "2", "2", "12"]);
}

/// DataFrame column verbs reached through an *untyped* helper parameter
/// (static type Unknown) must dispatch like the walker: a `@column` argument
/// can only mean a column verb, so it routes to the runtime-validated ops
/// instead of mis-compiling as an array comprehension (`where`) or a value
/// method (`sort`, grouped `mean`). On a `missing` receiver the walker's two
/// routes differ: `where`/`filter` propagate with the predicate untouched,
/// while `sort`/aggregations evaluate arguments first — so their `@col` raises
/// the column-reference error. Both must match exactly.
#[test]
fn unknown_receiver_dataframe_verbs_match_walker() {
    let col_err = "`@a` is a column reference, only valid inside a DataFrame operation";
    let src = "fn w(d) = d.where(@a > 1)\n\
fn s(x) = x.sort(@a)\n\
fn m(g) = g.mean(@a)\n\
df = dataframe({k: [1, 1, 2], a: [3.0, 1.0, 2.0]})\n\
print(w(df).count())\n\
print(s(df).count())\n\
print(m(df.group(@k)).count())\n\
print(w(missing))\n\
print((try s(missing)).error)\n\
print((try m(missing)).error)\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "unk_df_verbs");
        assert_eq!(code, Some(0), "stderr: {err}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["2", "3", "2", "missing", col_err, col_err], "env {env:?}");
    }
}

/// A `DataFrame`-annotated parameter fed `missing` at runtime propagates
/// missing (ADR-0001) instead of raising — annotations are advisory, and the
/// walker checks the receiver before dispatch.
#[test]
fn annotated_dataframe_verb_propagates_missing() {
    let src = "fn g(df: DataFrame) = df.where(@a > 1)\nprint(g(missing))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "annot_missing");
        assert_eq!(code, Some(0), "stderr: {err}");
        assert_eq!(out.trim(), "missing");
    }
}

/// A `mut` global read inside a fn body types as Unknown: the checker must not
/// freeze the definition-time type, because the global can be rebound to a
/// different type before the call (here DataFrame -> Array; the frozen type
/// mis-routed `where` to the DataFrame verb, which raised on the array).
#[test]
fn mut_global_rebound_type_matches_walker() {
    let src = "mut d = dataframe({a: [3, 1, 2]})\n\
fn g() = d.where(it > 1)\n\
d = [5, 1, 2]\n\
print(g())\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "mut_rebound");
        assert_eq!(code, Some(0), "stderr: {err}");
        assert_eq!(out.trim(), "[5, 2]");
    }
}

/// A top-level `fn` binds its name like any other definition: colliding with
/// an *immutable* global (seeded constant or user binding) raises at the
/// definition point on every engine; colliding with a *mutable* global
/// reassigns it — the function value wins and calls dispatch to it.
#[test]
fn fn_name_collision_with_global_matches_walker() {
    for (src, tag) in [
        ("fn inf(x) = x + 1\nprint(inf(1))\n", "fn_inf"),
        ("x = 5\nfn x(n) = n\nprint(x)\n", "fn_user"),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (_, err, code) = run_source(src, env, tag);
            assert_eq!(code, Some(1), "env {env:?} src {tag}");
            // `fn inf` gets the seeded-constant wording, `fn x` the generic one; the
            // shared clause is what this test pins (exact texts live in the corpus).
            assert!(
                err.contains("cannot be reassigned"),
                "env {env:?} stderr: {err}"
            );
        }
    }
    let ok = "mut f = 5\nfn f(x) = x * 2\nprint(f(3))\nf = 7\nprint(f)\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(ok, env, "fn_mut_global");
        assert_eq!(code, Some(0), "stderr: {err}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["6", "7"], "env {env:?}");
    }
}

/// A function body may call a peer defined BELOW it — mutual recursion, forward
/// references, longer cycles. The bytecode compiler used to resolve names in a
/// single pass, so `even` calling `odd` lowered to a raise while the tree-walker
/// (which resolves at call time) ran the same program fine: two engines
/// disagreeing about whether a program *exists*.
#[test]
fn mutual_recursion_and_forward_references_match_walker() {
    for (src, want, tag) in [
        (
            "fn even(n) = if n == 0 then true else odd(n - 1)\n\
fn odd(n) = if n == 0 then false else even(n - 1)\n\
print(even(4))\nprint(odd(7))\n",
            "true\ntrue",
            "mutual",
        ),
        ("fn a(n) = b(n)\nfn b(n) = n + 1\nprint(a(1))\n", "2", "forward"),
        (
            "fn f(n) = if n == 0 then 0 else g(n - 1)\n\
fn g(n) = if n == 0 then 1 else h(n - 1)\n\
fn h(n) = if n == 0 then 2 else f(n - 1)\nprint(f(7))\n",
            "1",
            "three_cycle",
        ),
        // A lambda inside a body inherits that body's forward visibility.
        ("fn a(xs) = xs.map(x => b(x))\nfn b(x) = x * 2\nprint(a([1, 2]))\n", "[2, 4]", "lambda_in_body"),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(src, env, tag);
            assert_eq!(code, Some(0), "env {env:?} src {tag} stderr: {err}");
            assert_eq!(out.trim(), want, "env {env:?} src {tag}");
        }
    }
}

/// ADR 0027: a top-level `fn` is FILE-SCOPED. It is callable above its own definition, and
/// a name that shadows a builtin means the user's function everywhere in the file — not the
/// builtin above the definition and the user's below.
///
/// This supersedes `forward_reference_does_not_leak_to_top_level_or_shadow_retroactively`,
/// which pinned the opposite. That test recorded the tree-walker's resolve-at-call-time
/// order-sensitivity, which is exactly the silent three-engine divergence the ADR removes:
/// `fn use(v) = round(v)` called either side of `fn round` answered `1, 1` compiled and
/// `1, 99` interpreted.
#[test]
fn a_top_level_fn_is_file_scoped_on_every_engine() {
    for (src, want, tag) in [
        // Callable above its own definition.
        ("print(f(1))\nfn f(x) = x + 1\n", "2", "call_above"),
        // A shadow is retroactive: the user's `round`, both times.
        ("print(round(1.4))\nfn round(x) = 99\nprint(round(1.4))\n", "99\n99", "shadow_above"),
        // The divergence that motivated the ADR — a body compiled above the shadow.
        (
            "fn use(v) = round(v)\nprint(use(1.4))\nfn round(x) = 99\nprint(use(1.4))\n",
            "99\n99",
            "body_above_shadow",
        ),
        // Mutual recursion between two builtin-shadowing names now resolves to the user's.
        (
            "fn round(n) = if n == 0 then 0 else abs(n - 1)\n\
fn abs(n) = if n == 0 then 1 else round(n - 1)\nprint(round(5))\n",
            "1",
            "mutual_builtin_names",
        ),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(src, env, tag);
            assert_eq!(code, Some(0), "{tag} env {env:?} stderr: {err}");
            assert_eq!(out.trim().replace("\r\n", "\n"), want, "{tag} env {env:?}");
        }
    }
    // A genuinely absent name still reports as absent (not as a reserved slot).
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source("fn a(n) = nosuch(n)\nprint(a(1))\n", env, "missing");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("`nosuch` is not a known function"), "env {env:?} stderr: {err}");
    }
}

/// Mutual tail recursion is constant-space. The peers are static callees now, so the
/// tail-call peephole applies to them exactly as it does to self-recursion; the old
/// way to write this (threading the peer as a PARAMETER) is a call through a value,
/// which is not a static tail call and still caps at the 20,000-frame limit.
#[test]
fn mutual_tail_recursion_is_constant_space() {
    let src = "fn a(n, s) = if n == 0 then s else b(n - 1, s + n)\n\
fn b(n, s) = if n == 0 then s else a(n - 1, s + n)\n\
print(a(1000000, 0))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(src, env, "mutual_tco");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.trim(), "500000500000", "env {env:?}");
    }
    let threaded = "fn t(n, acc, self) = if n == 0 then acc else self(n - 1, acc + n, self)\n\
print(t(25000, 0, t))\n";
    let (_, err, code) = run_source(threaded, &[], "threaded");
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(err.contains("maximum recursion depth"), "stderr: {err}");
}

/// Unbounded mutual recursion raises the depth guard rather than overflowing the
/// native stack. `jit::eligible_set` excludes every function on a recursion *cycle*
/// for exactly this reason, and its comment says the exclusion must not depend on
/// the front-end's define-before-use rule — "a front-end policy that could change".
/// Two-pass registration changed it, so the property is pinned here instead of
/// re-argued: a missing base case must stay a catchable Helix error, never a
/// killed host. Non-tail on purpose — a tail call becomes a loop and never returns.
#[test]
fn unbounded_mutual_recursion_raises_instead_of_crashing() {
    let src = "fn a(n) = 1 + b(n + 1)\nfn b(n) = 1 + a(n + 1)\nprint(a(0))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(src, env, "unbounded_mutual");
        // Some(1) is a clean Helix error; None would be death by signal.
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("maximum recursion depth"), "env {env:?} stderr: {err}");
    }
}

/// Passing an imported module's function to `map`/`any`/`all` APPLIES it. Only a bare
/// `Ident` used to be wrapped into `it => f(it)`, so a dotted path fell through to the
/// implicit-`it` body rule and mapped every element to the function VALUE:
/// `[<function/1>, <function/1>, <function/1>]`, exit 0, no diagnostic. Passing a library
/// function to `map` is the ordinary way to use a library.
#[test]
fn map_over_a_module_function_applies_it() {
    let dir = std::env::temp_dir().join("helix_modmap");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("util.helix"),
        "export fn double(x) = x * 2\nexport fn is_big(x) = x > 2\n",
    )
    .unwrap();
    let entry = dir.join("main.helix");
    std::fs::write(
        &entry,
        "import util\n\
print([1, 2, 3].map(util.double))\n\
print([1, 2, 3].any(util.is_big))\n\
rows = [{a: 1}, {a: 2}]\n\
print(rows.map(it.a))\n",
    )
    .unwrap();
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run(&[entry.to_str().unwrap()], env, "");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        // Line 3 is the guard that must NOT regress: `it.a` is a projection, not a call.
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec!["[2, 4, 6]", "true", "[1, 2]"],
            "env {env:?}"
        );
    }
}

/// A `do {}` binding may not reuse a `mut` global's name. The block desugars to
/// `let … in`, so `n = n + 1` bound a NEW immutable local and discarded it: `bump()`
/// twice printed `1`, `1`, and the global stayed `0` — exit 0, no diagnostic.
#[test]
fn do_binding_may_not_shadow_a_mut_global() {
    let bad = "mut n = 0\nfn bump() = do {\n  n = n + 1\n  n\n}\nprint(bump())\nprint(bump())\nprint(n)\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(bad, env, "do_mut_shadow");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("would shadow it, not update it"), "env {env:?} stderr: {err}");
    }
    // What must stay legal: both spellings that are unambiguous about binding.
    for (src, want, tag) in [
        // `do {}` over an IMMUTABLE global — shadowing is the documented behaviour.
        ("n = 5\nfn f() = do {\n  n = 1\n  n + 1\n}\nprint(f(), n)\n", "2 5", "do_imm"),
        // An explicit `let` over a `mut` global — the author wrote `let`, so they mean bind.
        ("mut n = 5\nfn f() = let n = 1 in n + 1\nprint(f(), n)\n", "2 5", "let_mut"),
        // Unrelated bindings in a file that has a mut global.
        ("mut n = 5\nfn f() = do {\n  a = 1\n  a + n\n}\nprint(f())\n", "6", "unrelated"),
    ] {
        let (out, err, code) = run_source(src, &[], tag);
        assert_eq!(code, Some(0), "{tag} stderr: {err}");
        assert_eq!(out.trim(), want, "{tag}");
    }
}

/// Two different modules that share a basename both bind that name; the LAST import
/// silently won, so `shared.who()` answered `B` with no diagnostic and reordering the
/// two import lines changed the program's output.
#[test]
fn two_modules_with_the_same_basename_are_rejected() {
    let dir = std::env::temp_dir().join("helix_modclash");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::write(dir.join("a/shared.helix"), "export fn who() = \"A\"\n").unwrap();
    std::fs::write(dir.join("b/shared.helix"), "export fn who() = \"B\"\n").unwrap();

    let clash = dir.join("clash.helix");
    std::fs::write(&clash, "import a.shared\nimport b.shared\nprint(shared.who())\n").unwrap();
    let (_, err, code) = run(&[clash.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(err.contains("already bound to a different module"), "stderr: {err}");

    // Aliasing is the fix the hint names, and it must work.
    let aliased = dir.join("aliased.helix");
    std::fs::write(
        &aliased,
        "import a.shared as x\nimport b.shared as y\nprint(x.who(), y.who())\n",
    )
    .unwrap();
    let (out, err, code) = run(&[aliased.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "A B");

    // Importing the SAME module twice binds the same thing — still fine.
    let twice = dir.join("twice.helix");
    std::fs::write(&twice, "import a.shared\nimport a.shared\nprint(shared.who())\n").unwrap();
    let (out, err, code) = run(&[twice.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "A");
}

/// A test file that runs to completion without asserting anything FAILS. Reporting `ok`
/// is how a whole file of `fn test_*` definitions that nobody calls reads as green — the
/// worst answer a test runner can give, because it is indistinguishable from real
/// coverage. `helix test` runs a file top to bottom; it has no collection phase.
#[test]
fn helix_test_fails_a_file_that_asserts_nothing() {
    let dir = std::env::temp_dir().join("helix_vacuous");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The pytest habit: defined, never called.
    std::fs::write(
        dir.join("habit_test.helix"),
        "fn test_reverse() = assert([1, 2].reverse() == [2, 1], \"reversed\")\n",
    )
    .unwrap();
    let (out, err, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "stderr: {err}\nout: {out}");
    assert!(out.contains("without asserting anything"), "out:\n{out}");
    // The diagnostic names the actual mistake rather than a generic one.
    assert!(out.contains("nothing calls them"), "out:\n{out}");

    // Calling it passes, and so does asserting at the top level.
    std::fs::write(
        dir.join("habit_test.helix"),
        "fn test_reverse() = assert([1, 2].reverse() == [2, 1], \"reversed\")\ntest_reverse()\n",
    )
    .unwrap();
    std::fs::write(dir.join("plain_test.helix"), "assert_eq(1 + 1, 2)\n").unwrap();
    let (out, err, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}\nout: {out}");
    assert!(out.contains("2 passed"), "out:\n{out}");

    // A real assertion failure is still a failure, not a vacuity report.
    std::fs::write(dir.join("plain_test.helix"), "assert(1 == 2, \"nope\")\n").unwrap();
    let (out, _, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "out: {out}");
    assert!(!out.contains("without asserting anything"), "out:\n{out}");
}

/// `helix test` runs the USER's `>>>` doc examples. `docs/comments-and-docs.md` sells
/// "a documented example is executed, on all three engines, every time" — and that was
/// true only of Helix's own source, checked by a `cargo test` nobody outside this repo
/// can run. For a library author writing `##  >>> …`, the examples were decoration.
#[test]
fn helix_test_runs_the_users_own_doc_examples() {
    let dir = std::env::temp_dir().join("helix_userdoc");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let lib = dir.join("lib.helix");
    let doc = |expect: &str| {
        format!(
            "## Doubles a number.\n\
             ##\n\
             ##     >>> double(21)\n\
             ##     {expect}\n\
             export fn double(x) = x * 2\n"
        )
    };

    // A correct example passes and counts as a test.
    std::fs::write(&lib, doc("42")).unwrap();
    let (out, err, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}\nout: {out}");
    assert!(out.contains("(doc)") && out.contains("1 passed"), "out:\n{out}");

    // A DRIFTED example fails, naming both sides. This is the entire value proposition.
    std::fs::write(&lib, doc("999")).unwrap();
    let (out, _, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "out:\n{out}");
    assert!(out.contains("expected: 999") && out.contains("got:      42"), "out:\n{out}");

    // An example resolves its module's own imports, because it runs beside the source.
    std::fs::write(dir.join("dep.helix"), "export fn tri(x) = x * 3\n").unwrap();
    std::fs::write(
        &lib,
        "import dep\n\
## Triples, then adds one.\n\
##\n\
##     >>> bump(2)\n\
##     7\n\
export fn bump(x) = dep.tri(x) + 1\n",
    )
    .unwrap();
    let (out, err, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}\nout: {out}");
    assert!(out.contains("1 passed"), "out:\n{out}");

    // A SCRIPT's examples are skipped — running it would re-run its side effects — and
    // the skip is REPORTED. A runner that quietly checks less than you think is the
    // failure mode this whole area is about.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("script.helix"),
        "## Doubles.\n\
##\n\
##     >>> double(21)\n\
##     42\n\
fn double(x) = x * 2\n\
print(\"a side effect\")\n",
    )
    .unwrap();
    let (out, _, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "out:\n{out}");
    assert!(out.contains("skipped doc examples in 1 file"), "out:\n{out}");
}

/// A package can read the data it ships. Every path resolved against the process's
/// working directory, and nothing exposed a module's own location — so a library could
/// not carry a scoring matrix, a codon table or a reference panel at all: the same
/// program worked from the project root and broke from a subdirectory.
#[test]
fn source_path_resolves_against_the_file_the_call_is_written_in() {
    let dir = std::env::temp_dir().join("helix_srcpath");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("lib/codons.csv"), "codon,aa\nATG,M\nTAA,*\n").unwrap();
    std::fs::write(
        dir.join("lib/bio.helix"),
        "export fn table() = read_csv(source_path(\"codons.csv\"))\n",
    )
    .unwrap();
    let entry = dir.join("app.helix");
    std::fs::write(&entry, "import lib.bio\nprint(bio.table().count())\n").unwrap();

    // The data file sits beside `bio.helix`, two directories from where the entry lives —
    // and the answer must not depend on where the process was started.
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run(&[entry.to_str().unwrap()], env, "");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.trim(), "2", "env {env:?}");
    }

    // An absolute path passes through untouched, so wrapping a caller's path is safe.
    let abs = dir.join("abs.helix");
    std::fs::write(&abs, "print(source_path(\"/etc/hostname\"))\n").unwrap();
    let (out, err, code) = run(&[abs.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "/etc/hostname");
}

/// A library rejecting its caller's argument can say so in its own words. The only
/// mechanism was `assert`, which hard-codes "assertion failed: " and cannot carry a
/// `help:` line — so a rejected argument read as a broken library. ADR 0004 leaves open
/// whether user-raised errors can be as instructive as the interpreter's own.
#[test]
fn raise_reports_a_domain_error_with_the_librarys_own_words() {
    let src = "fn go(path) =\n  \
if path.starts_with(\"/\") then path\n  \
else raise(\"route path must start with '/'\", \"pass a path like \\\"/admin\\\".\")\n\
print(go(\"admin\"))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(src, env, "raise_domain");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("error: route path must start with '/'"), "env {env:?}: {err}");
        assert!(!err.contains("assertion failed"), "env {env:?}: {err}");
        assert!(err.contains("help: pass a path like \"/admin\"."), "env {env:?}: {err}");
    }

    // It is an ORDINARY error: `try` catches it, like any other.
    let caught = "fn go(n) = raise(\"nope\", \"pass something else.\")\n\
r = try go(1)\nprint(r.ok, r.error)\n";
    let (out, err, code) = run_source(caught, &[], "raise_caught");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "false nope");

    // It never returns, so it types in a value position — `if bad then raise(…) else x`
    // is the shape every guard wants, and `Unit` would have rejected it.
    let positioned = "fn f(n) = if n > 0 then n else raise(\"must be positive\")\nprint(f(3))\n";
    let (out, err, code) = run_source(positioned, &[], "raise_pos");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "3");
}

/// Four day-one paper cuts, each of which sent a new user down a wrong path.
#[test]
fn day_one_paper_cuts_point_at_the_spelling_that_works() {
    // (a) `r.go(3)` — the object-API spelling everyone writes first. The old error named
    // `get`/`has`/`keys`/`values`/`items`, none of which is the fix.
    let rec = "r = {go: (n => n * 2), size: 3}\n";
    let (_, err, code) = run_source(&format!("{rec}print(r.go(3))\n"), &[], "rec_fn_field");
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(err.contains("`go` is a field of this record, not a method"), "{err}");
    assert!(err.contains("(rec.go)(…)"), "the help must name a working spelling: {err}");
    // A non-function field says the other true thing: drop the parentheses.
    let (_, err, code) = run_source(&format!("{rec}print(r.size())\n"), &[], "rec_val_field");
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(err.contains("read it without parentheses"), "{err}");
    // And the three spellings that work still do.
    let (out, err, code) =
        run_source(&format!("{rec}g = r.go\nprint((r.go)(3), g(3), r[\"go\"](3))\n"), &[], "rec_ok");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "6 6 6");

    // (b) `mut` in a body said "unexpected `mut`", which reads as "not supported, give up".
    // The thing the author wants already works, spelled without the keyword.
    for (src, tag) in [
        ("fn f() = do {\n  mut n = 0\n  n + 1\n}\nprint(f())\n", "do_mut"),
        ("fn f() = let mut n = 1 in n\nprint(f())\n", "let_mut"),
    ] {
        let (_, err, code) = run_source(src, &[], tag);
        assert_eq!(code, Some(1), "{tag} stderr: {err}");
        assert!(err.contains("`mut` declares a top-level binding"), "{tag}: {err}");
        assert!(err.contains("rebinds by name"), "{tag}: {err}");
    }
    // The idiom the help names must actually work.
    let (out, err, code) = run_source(
        "fn f() = do {\n  n = 0\n  n = n + 1\n  n = n * 10\n  n\n}\nprint(f())\n",
        &[],
        "do_rebind",
    );
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "10");

    // (c) `chars()`. The old linear-time spelling was `s.replace("", "\t").split("\t")` —
    // undiscoverable, and WRONG: it yields an empty string at each end.
    let (out, err, code) = run_source(
        "print(\"hello\".chars())\nprint(\"héllo\".chars().count())\n\
print(\"hello\".chars().filter(it != \"l\").count())\n",
        &[],
        "chars",
    );
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.lines().collect::<Vec<_>>(), vec!["[\"h\", \"e\", \"l\", \"l\", \"o\"]", "5", "3"]);
}

/// A facade re-export keeps the target's defaults and named arguments. `export greet =
/// inner.greet` binds the function VALUE, so its signature lived only in `inner` and was
/// lost exactly one hop out — making a package's front door either impossible or a
/// hand-copied wrapper that rots when the target's parameters change.
#[test]
fn a_facade_re_export_keeps_defaults_and_named_arguments() {
    let dir = std::env::temp_dir().join("helix_facade");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inner.helix"), "export fn scale(x, by: Int = 2) = x * by\n").unwrap();
    std::fs::write(dir.join("facade.helix"), "import inner\nexport scale = inner.scale\n").unwrap();
    // A facade OF a facade, to prove the alias is followed transitively.
    std::fs::write(dir.join("outer.helix"), "import facade\nexport scale = facade.scale\n").unwrap();
    // A re-exported non-function value keeps working.
    std::fs::write(dir.join("konst.helix"), "export LIMIT = 42\n").unwrap();
    std::fs::write(dir.join("kf.helix"), "import konst\nexport LIMIT = konst.LIMIT\n").unwrap();

    for (module, want) in [("facade", "10\n50"), ("outer", "10\n50")] {
        let entry = dir.join(format!("use_{module}.helix"));
        std::fs::write(
            &entry,
            format!("import {module}\nprint({module}.scale(5))\nprint({module}.scale(5, by: 10))\n"),
        )
        .unwrap();
        let (out, err, code) = run(&[entry.to_str().unwrap()], &[], "");
        assert_eq!(code, Some(0), "{module} stderr: {err}");
        assert_eq!(out.trim().replace("\r\n", "\n"), want, "{module}");
    }
    let entry = dir.join("use_k.helix");
    std::fs::write(&entry, "import kf\nprint(kf.LIMIT)\n").unwrap();
    let (out, err, code) = run(&[entry.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "42");
}

/// A `reduce` body may call a user function with a MIXED (float-parameter) signature, the
/// way a `map` body already could. The same integrand was 1.844s as a reduce against 0.031s
/// as a map — and, tellingly, equal to its own `HELIX_NOJIT` time, because the kernel
/// declined outright rather than compiling a slow one. Now 0.051s, a 49x JIT gain.
///
/// These pin the ANSWERS. Agreement is the property that matters: a kernel that computes
/// something different from the VM is worse than no kernel.
#[test]
fn a_reduce_body_may_call_a_mixed_specialization() {
    for (src, want, tag) in [
        // The integrand shape the whole change is for.
        (
            "fn f(x) = x * x * 0.5 + x\n\
print(range(0, 1000).reduce(0.0, (a, i) => a + f(to_float(i))))\n",
            "166916250.0",
            "integrand",
        ),
        // Two different mixed callees in one body.
        (
            "fn f(x) = x * 2.0\nfn g(x) = x * x\n\
print(range(0, 100).reduce(0.0, (a, i) => a + f(to_float(i)) + g(to_float(i))))\n",
            "338250.0",
            "two_callees",
        ),
        // A mixed call nested inside another mixed call's argument — the inner one writes
        // the same poison cell, so it must fold its flag before the outer re-zeroes it.
        (
            "fn f(x) = x * 2.0\nfn g(x) = x + 1.0\n\
print(range(0, 100).reduce(0.0, (a, i) => a + f(g(to_float(i)))))\n",
            "10100.0",
            "nested",
        ),
        // A mixed callee whose result is Int-rooted (bitcast back as i64, not f64).
        (
            "fn f(x) = to_int(x * 2.0)\n\
print(range(0, 100).reduce(0.0, (a, i) => a + to_float(f(to_float(i)))))\n",
            "9900.0",
            "int_result",
        ),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(src, env, tag);
            assert_eq!(code, Some(0), "{tag} env {env:?} stderr: {err}");
            assert_eq!(out.trim(), want, "{tag} env {env:?}");
        }
    }
}

/// A mixed callee that RAISES inside a reduce is not swallowed. This is why the poison
/// decision had to become a stored fact FIRST: the kernel's signature and the VM's call
/// wrapper are chosen from the same `ReduceLoop::raises`, so a callee that bails has
/// somewhere to put its flag and the VM re-runs the bytecode loop for the exact error.
/// Built poison-free instead, these would print a number and exit 0.
#[test]
fn a_raising_mixed_callee_in_a_reduce_is_not_swallowed() {
    // A NaN reaches an ordering comparison INSIDE the callee.
    let nan_compare = "fn f(x) = if x < 1.0 then 0.0 else x\n\
print(range(0, 10).reduce(0.0, (a, i) => a + f(sqrt(to_float(i) - 5.0))))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(nan_compare, env, "reduce_nan");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("cannot compare these values"), "env {env:?}: {err}");
    }
    // A rounder leaves i64 range INSIDE the callee.
    let rounder = "fn f(x) = to_float(floor(x))\n\
print(range(0, 4).reduce(0.0, (a, i) => a + f(to_float(i) * 1.0e300)))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(rounder, env, "reduce_rounder");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("cannot produce an integer"), "env {env:?}: {err}");
    }
    // The same callee never fed a raising value still computes — the bail is not a blanket
    // decline, and a kernel that always poisoned would pass the two checks above while
    // quietly giving up every speedup this change exists for.
    let clean = "fn f(x) = if x < 1.0 then 0.0 else x\n\
print(range(6, 16).reduce(0.0, (a, i) => a + f(sqrt(to_float(i) - 5.0))))\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(clean, env, "reduce_clean");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.trim(), "22.4682781862041", "env {env:?}");
    }
}

/// Growing an array in a fold is LINEAR. `acc.concat([x])` copied the whole accumulator
/// every iteration, because `LoadLocal` clones and the slot therefore still held a
/// reference at the moment `concat` ran — so the `Rc` could never be unique. 256k appends
/// took 6.5s. `Op::ConcatIntoLocal` takes the accumulator out of its slot and puts the
/// result back as ONE op, so the append can extend in place.
///
/// These pin the ANSWERS, on all three engines. The tree-walker has no such op, which makes
/// it a genuinely independent check rather than a restatement of the same code.
#[test]
fn growing_an_array_in_a_fold_matches_the_walker() {
    for (src, want, tag) in [
        ("print(range(0, 5).reduce([], (acc, i) => acc.concat([i])))", "[0, 1, 2, 3, 4]", "ints"),
        (
            "print(range(0, 4).reduce([], (acc, i) => acc.concat([to_float(i) * 0.5])))",
            "[0.0, 0.5, 1.0, 1.5]",
            "floats",
        ),
        (
            "print([\"a\", \"b\"].reduce([], (acc, s) => acc.concat([s, s])))",
            "[\"a\", \"a\", \"b\", \"b\"]",
            "strings",
        ),
        // Mixed element kinds must NOT take the packed fast path — `array_sniff` would
        // leave a different representation than extending in place does.
        (
            "print(range(0, 3).reduce([], (acc, i) => acc.concat([i, to_float(i)])))",
            "[0, 0.0, 1, 1.0, 2, 2.0]",
            "mixed_kinds",
        ),
        // The argument is compiled BEFORE the take, so it still sees the live accumulator.
        (
            "print(range(0, 4).reduce([], (acc, i) => acc.concat([acc.count()])))",
            "[0, 1, 2, 3]",
            "arg_reads_acc",
        ),
        // The argument RETAINS the accumulator, so the `Rc` is shared and the in-place path
        // must decline. Getting this wrong would alias a value into itself.
        (
            "print(range(0, 3).reduce([], (acc, i) => acc.concat([acc])))",
            "[[], [[]], [[], [[]]]]",
            "arg_nests_acc",
        ),
        // Multi-argument `concat` is not the recognised shape and takes the ordinary path.
        (
            "print(range(0, 3).reduce([], (acc, i) => acc.concat([i], [i * 10])))",
            "[0, 0, 1, 10, 2, 20]",
            "concat_many",
        ),
        // A `concat` whose receiver is NOT the accumulator must be left alone.
        (
            "xs = [9]\nprint(range(0, 3).reduce([], (acc, i) => xs.concat([i])))",
            "[9, 2]",
            "not_the_acc",
        ),
        ("print(range(0, 4).reduce([], (acc, i) => acc.concat(acc)))", "[]", "acc_twice"),
        ("print(range(0, 3).reduce([], (acc, i) => acc.concat([])).count())", "0", "empty_adds"),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(&format!("{src}\n"), env, tag);
            assert_eq!(code, Some(0), "{tag} env {env:?} stderr: {err}");
            assert_eq!(out.trim(), want, "{tag} env {env:?}");
        }
    }
    // Both error paths must read the accumulator while it is still in its slot, so the
    // wording is the walker's and a failed append leaves the frame as it found it.
    for (src, needle, tag) in [
        (
            "print(range(0, 3).reduce([], (acc, i) => acc.concat(i)))",
            "`concat` expects arrays",
            "non_array_arg",
        ),
        (
            "print(range(0, 3).reduce(0, (acc, i) => acc.concat([i])))",
            "has no method `concat`",
            "non_array_acc",
        ),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (_, err, code) = run_source(&format!("{src}\n"), env, tag);
            assert_eq!(code, Some(1), "{tag} env {env:?} stderr: {err}");
            assert!(err.contains(needle), "{tag} env {env:?}: {err}");
        }
    }
}

/// Building a dictionary in a fold is linear. `insert` clones the whole `BTreeMap` per
/// call, which made it the worse of the two: 8,000 inserts cost 0.25s where 8,000 appends
/// cost 0.013s. ADR 0020 names this fast path as future work.
#[test]
fn building_a_dict_in_a_fold_matches_the_walker() {
    for (src, want, tag) in [
        (
            "print(range(0, 4).reduce(dict(), (acc, i) => acc.insert(i, i * 2)))",
            "{0 => 0, 1 => 2, 2 => 4, 3 => 6}",
            "int_keys",
        ),
        (
            "print([\"a\", \"b\"].reduce(dict(), (acc, s) => acc.insert(s, s)))",
            "{\"a\" => \"a\", \"b\" => \"b\"}",
            "string_keys",
        ),
        // A repeated key overwrites, in iteration order.
        (
            "print(range(0, 5).reduce(dict(), (acc, i) => acc.insert(i % 2, i)))",
            "{0 => 4, 1 => 3}",
            "overwrite",
        ),
        // The arguments are compiled before the take, so they see the live accumulator.
        (
            "print(range(0, 3).reduce(dict(), (acc, i) => acc.insert(i, acc.count())))",
            "{0 => 0, 1 => 1, 2 => 2}",
            "arg_reads_acc",
        ),
        // The VALUE retains the accumulator, so the in-place path must decline.
        (
            "print(range(0, 2).reduce(dict(), (acc, i) => acc.insert(i, acc)))",
            "{0 => {}, 1 => {0 => {}}}",
            "value_nests_acc",
        ),
        // An `insert` whose receiver is not the accumulator is left alone.
        (
            "d0 = dict()\nprint(range(0, 3).reduce(dict(), (acc, i) => d0.insert(i, i)))",
            "{2 => 2}",
            "not_the_acc",
        ),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(&format!("{src}\n"), env, tag);
            assert_eq!(code, Some(0), "{tag} env {env:?} stderr: {err}");
            assert_eq!(out.trim(), want, "{tag} env {env:?}");
        }
    }
    for (src, needle, tag) in [
        (
            "print(range(0, 2).reduce(dict(), (acc, i) => acc.insert([i], i)))",
            "a dict key must be",
            "bad_key",
        ),
        (
            "print(range(0, 2).reduce(0, (acc, i) => acc.insert(i, i)))",
            "has no method `insert`",
            "non_dict_acc",
        ),
    ] {
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (_, err, code) = run_source(&format!("{src}\n"), env, tag);
            assert_eq!(code, Some(1), "{tag} env {env:?} stderr: {err}");
            assert!(err.contains(needle), "{tag} env {env:?}: {err}");
        }
    }
}

/// A library's PRIVATE sibling import cannot be captured by what the consumer installs.
///
/// The dependency map is the ROOT project's and is consulted for every file in the graph,
/// including files inside a dependency, which have no say in it. With dependency keys
/// winning over local siblings, a consumer adding an unrelated package named `helpers`
/// silently rewired a correct, self-contained library's internals: `mathlib.go(10)` returned
/// 20 alone and 1010 in that consumer. Exit 0, `helix check` ok, and all three engines agreed
/// — because all three were equally wrong, so the differential oracle is blind to it. This
/// test is the only thing that catches it.
#[test]
fn a_consumers_dependency_cannot_capture_a_librarys_private_import() {
    let dir = std::env::temp_dir().join("helix_import_capture");
    let _ = std::fs::remove_dir_all(&dir);
    for p in ["mathlib", "helpers", "appA", "appB"] {
        std::fs::create_dir_all(dir.join(p)).unwrap();
    }
    // A self-contained library with a private sibling it imports by name.
    std::fs::write(dir.join("mathlib/helix.toml"), "[package]\nname = \"mathlib\"\n").unwrap();
    std::fs::write(dir.join("mathlib/helpers.helix"), "export fn scale(x) = x * 2\n").unwrap();
    std::fs::write(
        dir.join("mathlib/mathlib.helix"),
        "import helpers\nexport fn go(x) = helpers.scale(x)\n",
    )
    .unwrap();
    // An unrelated package that happens to be named `helpers`.
    std::fs::write(dir.join("helpers/helix.toml"), "[package]\nname = \"helpers\"\n").unwrap();
    std::fs::write(dir.join("helpers/helpers.helix"), "export fn scale(x) = x * 100 + 10\n")
        .unwrap();

    let app = |name: &str, deps: &str| {
        std::fs::write(
            dir.join(name).join("helix.toml"),
            format!("[package]\nname = \"{name}\"\n\n[dependencies]\n{deps}"),
        )
        .unwrap();
        std::fs::write(dir.join(name).join("main.helix"), "import mathlib\nprint(mathlib.go(10))\n")
            .unwrap();
        let entry = dir.join(name).join("main.helix");
        run(&[entry.to_str().unwrap()], &[], "")
    };

    let (out, err, code) = app("appA", "mathlib = { path = \"../mathlib\" }\n");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "20", "the library alone");

    // The SAME library, in a consumer that also depends on something named `helpers`.
    let (out, err, code) = app(
        "appB",
        "mathlib = { path = \"../mathlib\" }\nhelpers = { path = \"../helpers\" }\n",
    );
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "20", "a consumer's dependency must not rewire mathlib's internals");
}

/// A DataFrame query's bare name resolves to a BINDING IN SCOPE before a column (ADR 0028),
/// so a library's parameter names are not reserved words in the caller's data.
///
/// This was the last known silent wrong answer: `above(df, 3)` returned 2 on columns
/// {value, other} and 3 on {value, cutoff} — `cutoff` bound to the caller's column, turning
/// the predicate into a column-vs-column comparison. Exit 0, `helix check` ok, three engines
/// agreeing because all three were equally wrong.
#[test]
fn a_query_binds_a_name_to_a_local_before_a_column() {
    let lib = "fn above(frame, cutoff) = frame.where(@value > cutoff).count()\n";
    // The ONLY difference between these is the second column's NAME.
    for (cols, tag) in [("other", "no_clash"), ("cutoff", "clashes_with_param")] {
        let src = format!(
            "{lib}df = dataframe({{value: [1, 5, 9], {cols}: [0, 0, 0]}})\nprint(above(df, 3))\n"
        );
        for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
            let (out, err, code) = run_source(&src, env, tag);
            assert_eq!(code, Some(0), "{tag} env {env:?} stderr: {err}");
            assert_eq!(out.trim(), "2", "{tag} env {env:?}: the caller's schema changed the answer");
        }
    }
    // A bare name with NO binding in scope is still a column — the DSL's ergonomics are
    // untouched for the case they exist to serve.
    let plain = "df = dataframe({value: [1, 5, 9]})\nprint(df.where(value > 3).count())\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(plain, env, "bare_is_column");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.trim(), "2", "env {env:?}");
    }
    // `@name` still pins the column side explicitly, even when a local shadows it.
    let pinned =
        "cutoff = 99\ndf = dataframe({cutoff: [1, 5, 9]})\nprint(df.where(@cutoff > 3).count())\n";
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (out, err, code) = run_source(pinned, env, "sigil_pins_column");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.trim(), "2", "env {env:?}");
    }
}

/// A deep `x => x => ...` lambda chain must hit the parser depth cap with a
/// clean error — lambda bodies were the one expr() recursion that skipped the
/// depth counter, so 2000 nestings overflowed the native stack (SIGABRT).
#[test]
fn deep_lambda_chain_errors_cleanly() {
    let deep = format!("f = {}1\nprint(1)\n", "x => ".repeat(2000));
    let (_, err, code) = run_source(&deep, &[], "deep_lambda");
    assert_eq!(code, Some(1), "expected a clean parse error, stderr: {err}");
    assert!(err.contains("nested or chained too deeply"), "stderr: {err}");
    let ok = "f = x => x => 1\nprint((f(0))(0))\n";
    let (out, err, code) = run_source(ok, &[], "shallow_lambda");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "1");
}

/// A runtime error inside an interpolation hole reports the interpolated
/// string's real source position — identically on every engine. Holes are
/// parsed as standalone snippets; before the parse-time relocation the walker
/// pointed at the snippet's line 1 and the VM at the op's 0:0.
#[test]
fn interp_hole_errors_point_at_the_string_on_all_engines() {
    let src = "s = \"hi\"\nprint(\"val is {s:.2f} ok\")\n";
    let mut errs = Vec::new();
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source(src, env, "hole_pos");
        assert_eq!(code, Some(1), "env {env:?}");
        errs.push(err);
    }
    assert_eq!(errs[0], errs[1], "JIT vs VM");
    assert_eq!(errs[1], errs[2], "VM vs tree-walker");
    assert!(errs[0].contains("cannot format a String"), "stderr: {}", errs[0]);
    assert!(errs[0].contains(":2:7"), "should point at the string on line 2: {}", errs[0]);
}

/// `helix emit-hbc` end-to-end (ADR 0023): the subcommand had unit tests for
/// serialization but zero CLI coverage — arg parsing, file emission, `--dump`,
/// and the failure paths were unexercised.
#[test]
fn emit_hbc_writes_container_and_reports_errors() {
    let dir = std::env::temp_dir();
    let src_path = dir.join("helix_hbc_cli.helix");
    std::fs::write(
        &src_path,
        "fn compute(n) = if n <= 1 then n else compute(n - 1) + compute(n - 2)\nprint(compute(10))\n",
    )
    .unwrap();
    let out_path = dir.join("helix_hbc_cli.hbc");
    // `-o` writes a non-empty container and reports the entry mapping.
    let (out, err, code) = run(
        &[
            "emit-hbc",
            src_path.to_str().unwrap(),
            "--entry",
            "compute",
            "-o",
            out_path.to_str().unwrap(),
        ],
        &[],
        "",
    );
    assert_eq!(code, Some(0), "stderr: {err}");
    assert!(out.contains("wrote"), "stdout: {out}");
    assert!(out.contains("compute"), "entry map should name the entry: {out}");
    assert!(!std::fs::read(&out_path).unwrap().is_empty());
    // `--dump` prints the compiled instruction stream (a debugging aid).
    let (_, err2, code2) = run(
        &[
            "emit-hbc",
            src_path.to_str().unwrap(),
            "--entry",
            "compute",
            "--dump",
            "-o",
            out_path.to_str().unwrap(),
        ],
        &[],
        "",
    );
    assert_eq!(code2, Some(0), "stderr: {err2}");
    assert!(err2.contains("compiled program"), "stderr: {err2}");
    // Unknown flags and a missing script path fail cleanly.
    let (_, err3, code3) = run(&["emit-hbc", src_path.to_str().unwrap(), "--frobnicate"], &[], "");
    assert_eq!(code3, Some(1));
    assert!(err3.contains("unknown option"), "stderr: {err3}");
    let (_, err4, code4) = run(&["emit-hbc"], &[], "");
    assert_eq!(code4, Some(1));
    assert!(err4.contains("needs a script path"), "stderr: {err4}");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&out_path);
}

/// The `net` effect class is really gated under `HELIX_CAP=enforce` — the
/// existing capability matrix only exercised fs-read/-write, so a regression
/// ungating a whole class would have passed CI. Denial happens BEFORE any
/// connection attempt (no live endpoint needed); with the grant, the same call
/// proceeds to a plain transport error (nothing listens on the discard port).
#[test]
fn capability_enforce_gates_net_class() {
    let src = "r = try http_get(\"http://127.0.0.1:9/nope\")\nprint(r.ok)\nprint(r.error)\n";
    let (out, err, code) = run_source(src, &[("HELIX_CAP", "enforce")], "cap_net_deny");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert!(out.contains("false"), "stdout: {out}");
    assert!(out.contains("capability denied"), "expected the deny message: {out}");
    let (out2, err2, code2) = run_source(
        src,
        &[("HELIX_CAP", "enforce"), ("HELIX_ALLOW_NET", "on")],
        "cap_net_grant",
    );
    assert_eq!(code2, Some(0), "stderr: {err2}");
    assert!(out2.contains("false"), "stdout: {out2}");
    assert!(
        !out2.contains("capability denied"),
        "grant should open the gate (transport error instead): {out2}"
    );
}

/// THE ANTI-DRIFT CORPUS: every Helix program under `tests/corpus/` runs on
/// all three engines; the outputs must be (a) byte-identical across engines
/// and (b) equal to the checked-in `.expected` golden (exit code + stdout +
/// stderr, with the absolute source path normalized to `<src>`). Every
/// verified fix from the 2026-07 sweeps lives here as a runnable program, so
/// a future change that re-breaks one fails THIS test with the program name.
/// After an INTENTIONAL behavior change, regenerate goldens with
/// `UPDATE_CORPUS=1 cargo test corpus_is_engine_identical_and_pinned`.
#[test]
fn corpus_is_engine_identical_and_pinned() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let update = std::env::var("UPDATE_CORPUS").is_ok();
    let mut programs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/corpus exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "helix"))
        .collect();
    programs.sort();
    assert!(programs.len() >= 40, "corpus unexpectedly small: {}", programs.len());
    for path in programs {
        let rel = path.to_str().unwrap();
        let name = path.file_stem().unwrap().to_str().unwrap();
        let render = |env: &[(&str, &str)]| -> String {
            let (out, err, code) = run(&[rel], env, "");
            format!(
                "exit: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                out.trim_end_matches('\n'),
                err.replace(rel, "<src>").trim_end_matches('\n'),
            )
            .trim_end()
            .to_string()
        };
        let jit = render(&[]);
        let vm = render(&[("HELIX_NOJIT", "1")]);
        let tw = render(&[("HELIX_NOVM", "1")]);
        assert_eq!(jit, vm, "corpus `{name}`: JIT vs VM diverge");
        assert_eq!(vm, tw, "corpus `{name}`: VM vs tree-walker diverge");
        let golden_path = path.with_extension("expected");
        if update {
            std::fs::write(&golden_path, format!("{jit}\n")).unwrap();
            continue;
        }
        let golden = std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|_| panic!("missing golden for corpus `{name}`"));
        assert_eq!(
            jit,
            golden.trim_end_matches('\n'),
            "corpus `{name}` drifted from its golden — if the change is \
             intentional, regenerate with UPDATE_CORPUS=1"
        );
    }
}

/// The `Dna` invariant holds at the FILE BOUNDARY, not just at `dna()`.
///
/// `read_fasta`/`read_fastq` used to uppercase without validating, minting `Dna`
/// values that `dna()` itself rejects — and the sequence methods, written against
/// the invariant, then answered with plausible nonsense instead of erroring: a
/// `>s1 / ATGCXXZZ!!` record gave `gc_content() = 0.2` and `kmers(3)` returned 2
/// k-mers where a 10-base sequence must yield 8. A corrupt FASTA produced a
/// believable GC number and no warning. Both readers now apply `dna()`'s exact
/// rule, and real-world FASTA (lowercase soft-masking, `N`, IUPAC codes) must
/// still read — that is the half a naive tightening would break.
#[test]
fn fasta_enforces_the_dna_invariant_at_the_boundary() {
    let dir = std::env::temp_dir().join("helix_bio_inv");
    std::fs::create_dir_all(&dir).unwrap();

    // (1) Bases `dna()` rejects must NOT become a Dna value.
    let bad = dir.join("bad.fasta");
    std::fs::write(&bad, ">s1\nATGCXXZZ!!\n").unwrap();
    let src = format!("r = try read_fasta(\"{}\")\nprint(r.ok)\nprint(r.error)\n", bad.display());
    let (out, err, code) = run_source(&src, &[], "bio_inv_bad");
    assert_eq!(code, Some(0), "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "false", "a protein/corrupt FASTA must not yield a Dna: {out}");
    assert!(lines[1].contains("not a valid DNA base"), "got: {}", lines[1]);
    assert!(lines[1].contains("s1"), "the error should name the record: {}", lines[1]);
    assert!(lines[1].contains("position 4"), "and the position: {}", lines[1]);

    // (2) Real-world FASTA still reads: lowercase soft-masking, N, IUPAC codes.
    let good = dir.join("good.fasta");
    std::fs::write(&good, ">ok1\natgcRYKM\n>ok2\nACGTNNNN\n").unwrap();
    let src = format!(
        "f = read_fasta(\"{}\")\nprint(f.count())\nprint(f[0].seq)\nprint(f[1].seq.gc_content())\n",
        good.display()
    );
    let (out, err, code) = run_source(&src, &[], "bio_inv_good");
    assert_eq!(code, Some(0), "stderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["2", "ATGCRYKM", "0.5"], "valid FASTA must still read: {out}");

    // (3) Whatever a reader produces, `dna()` must accept — the round-trip that
    //     the old readers broke.
    let src = format!(
        "f = read_fasta(\"{}\")\nprint((try dna(\"{{f[0].seq}}\")).ok)\n",
        good.display()
    );
    let (out, err, code) = run_source(&src, &[], "bio_inv_roundtrip");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "true", "a reader's Dna must round-trip through dna(): {out}");

    // (4) Same rule for FASTQ.
    let badq = dir.join("bad.fastq");
    std::fs::write(&badq, "@r1\nATGCZ\n+\n!!!!!\n").unwrap();
    let src = format!("r = try read_fastq(\"{}\")\nprint(r.ok)\n", badq.display());
    let (out, err, code) = run_source(&src, &[], "bio_inv_fastq");
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out.trim(), "false", "FASTQ must enforce the same invariant: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0024's never-abort property, enforced by CI instead of re-audited by hand.
///
/// A `.unwrap()`/`.expect()` on a path that user input can reach is a process
/// ABORT — uncatchable by `try`, no error message, no line number. This session's
/// audits found three of exactly that class ([10, 20][1::i64::MAX] wrapping a
/// slice cursor into a 2^63 index; `i64::MIN % -1`; a NaN comparator making
/// Rust's sort panic). Each was found by a human hunt months apart, which does
/// not scale.
///
/// This is a RATCHET, not a ban. The ~90 existing calls are proven-by-
/// construction, not sloppiness: 38 of vm.rs's are `stack.pop().unwrap()`, sound
/// because the compiler emits balanced code; the rest are guarded (a
/// `get_mut(name).unwrap()` after a contains-check) or genuine invariants
/// (`expect("same length as source tensor")`). Denying them outright would mean
/// ~90 `#[allow]`s — churn that buys no safety. What actually matters is that the
/// number cannot silently GROW: a new panicking call in a user-reachable path
/// must be a deliberate, reviewed act.
///
/// TO RAISE A BUDGET: prove the call cannot panic on ANY user input, say so in a
/// comment at the site, and bump the number here in the same commit — so the
/// justification lands in review next to the risk. If it can panic on some input,
/// it is a bug: return a `HelixError` instead.
#[test]
fn no_new_panicking_calls_on_user_reachable_paths() {
    // Files user input flows through. (Test modules live in their own files and
    // are excluded — a panicking assert in a test is the point of a test.)
    const BUDGET: &[(&str, usize)] = &[
        ("src/interp.rs", 7),
        ("src/interp/methods.rs", 1),
        ("src/interp/ops.rs", 3),
        ("src/interp/access.rs", 1),
        ("src/interp/builtins.rs", 8),
        // 7: `fold_take_append`'s two `env.get_mut(pa).unwrap()`s plus
        // `fold_append_str`'s one — the reduce arm inserts both binders
        // unconditionally before the loop and nothing removes them until the restore
        // after it, the same invariant the loop's own pre-existing
        // `get_mut(pa)/get_mut(pb)` unwraps rely on.
        ("src/interp/comprehensions.rs", 7),
        ("src/interp/dataframe_ops.rs", 0),
        // +3 (52 → 55): `TryJitScan`'s three operand pops. Emitted at exactly one compiler
        // site, which pushes `[start, end, init]` immediately before the op — proven at the
        // call site in `vm.rs`.
        // 56: +1 for `CompFindTest`'s operand pop, proved at the site — it is emitted at
        // exactly one place, right after the predicate body is compiled, so the stack
        // always holds that value. Same shape and same proof as `CompBoolTest` beside it.
        // 56 -> 57: the f64 filter dispatch's `stack.pop().unwrap()`, proven by the
        // `stack.last()` pattern match immediately above it (same argument as the Ints
        // arm's pop).
        // 60: the argument pops of `Op::ConcatIntoLocal` (1) and `Op::InsertIntoLocal` (2),
        // proved at each site — the compiler emits those argument expressions immediately
        // before the op, the same stack-shape invariant the other 57 rely on.
        ("src/vm.rs", 60),
        ("src/bytecode.rs", 1),
        ("src/bytecode/comprehensions.rs", 0),
        ("src/bytecode/ops.rs", 0),
        ("src/lexer.rs", 0),
        // 6: `desugar_position` no longer pops its predicate out of the arg vector — it
        // passes the args through untouched, so the `unwrap` went with it.
        ("src/parser.rs", 6),
        ("src/bio.rs", 0),
        ("src/value.rs", 0),
        ("src/jit.rs", 1),
        ("src/jit/ffi.rs", 0),
        ("src/types.rs", 0),
        ("src/types/synth.rs", 0),
        ("src/strfmt.rs", 0),
        ("src/module.rs", 2),
        ("src/sam.rs", 11),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut over: Vec<String> = Vec::new();
    let mut under: Vec<String> = Vec::new();
    for (rel, budget) in BUDGET {
        let path = root.join(rel);
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            // A budgeted file that vanished means the list is stale — say so
            // rather than silently passing.
            Err(_) => {
                over.push(format!("{rel}: budgeted but missing — update BUDGET"));
                continue;
            }
        };
        let n = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
            })
            .filter(|l| l.contains(".unwrap()") || l.contains(".expect("))
            .count();
        if n > *budget {
            over.push(format!(
                "{rel}: {n} panicking calls, budget {budget} (+{})",
                n - budget
            ));
        } else if n < *budget {
            under.push(format!("{rel}: {n}, budget {budget} — lower it to {n}"));
        }
    }
    assert!(
        over.is_empty(),
        "NEW panicking call(s) on a user-reachable path — ADR 0024 says user input must never \
         abort the host:\n  {}\n\nIf the call genuinely cannot panic on any input, prove it in a \
         comment at the site and raise the budget in the same commit. If it can, return a \
         HelixError instead.",
        over.join("\n  ")
    );
    // The ratchet only ratchets if it tightens when code improves.
    assert!(
        under.is_empty(),
        "panicking calls were REMOVED — tighten the budget so they cannot come back:\n  {}",
        under.join("\n  ")
    );
}

/// `HELIX_THREADS` caps the worker pool. It must be a pure CPU/latency control: the OUTPUT is
/// identical at every thread count, because parallel `map`/`filter` are elementwise, float
/// reductions are never reassociated (that would change the last bits and break the three-engine
/// oracle), and the parallel nested reduce partitions over independent outer indices and collects
/// in order.
///
/// This exists because the parallelism used to be imposed rather than chosen: the pool could only
/// be resized through rayon's own `RAYON_NUM_THREADS`, an implementation detail a Helix user has
/// no reason to know. The trade it controls is real and workload-dependent — measured on a 6-core
/// box, compute-bound work scales 5.4× for +4% total CPU while allocation-bound work gains only
/// 1.75× for +79% CPU — which is why this is a setting and not a different hard-coded default.
///
/// SCOPE, stated precisely so this is not mistaken for more than it is. The parallel branch is
/// selected by ARRAY LENGTH (`PAR_MATH_THRESHOLD`), not by worker count, so with
/// `HELIX_THREADS=1` rayon still takes the parallel code path — just with one worker. This test
/// therefore CANNOT catch a deterministic chunking bug (one that mis-orders identically at every
/// thread count); the corpus goldens and the three-engine oracle cover that. What it does catch is
/// any future change that makes a result depend on the worker count — e.g. parallelizing a float
/// reduce with a tree fold whose shape follows the split, which would move the last bits. That is
/// the property being pinned, and the wiring itself is verified separately: %CPU tracks the cap
/// (measured 99% / 195% / 374% / 446% at 1 / 2 / 4 / all on the all-pairs kernel).
#[test]
fn thread_count_changes_cpu_not_results() {
    // Shapes a bad chunking WOULD change: a parallel float map feeding an order-sensitive float
    // fold, a fused indexed map→reduce, a SAXPY sum with a float scalar capture, the parallel
    // nested reduce, and a filter whose output order must survive. Sizes are past
    // PAR_MATH_THRESHOLD (1<<15) so the parallel paths actually engage — below it everything is
    // serial and this would prove nothing.
    let progs = [
        "n = 200000\nxs = (0..n).map(i => i * 0.001)\nprint(xs.reduce(0.0, (s, x) => s + x))",
        "n = 200000\na = (0..n).map(i => i * 1.5)\nb = (0..n).map(i => i * 0.25)\nprint((0..n).map(i => a[i] + b[i]).reduce(0.0, (s, x) => s + x))",
        "n = 200000\nc = 2.5\na = (0..n).map(i => i * 1.5)\nprint((0..n).reduce(0.0, (s, i) => s + c * a[i]))",
        "n = 300\ncodes = (0..n).map(i => (i * 7) % 101)\nprint((0..n).map(i => (0..n).reduce(0, (acc, j) => acc + abs(codes[i] - codes[j]))).sum())",
        "n = 100000\nys = (0..n).map(i => i * 3).filter(x => x % 7 == 0)\nprint(\"{ys.length()} {ys.first()} {ys.last()}\")",
        "n = 200000\nprint((0..n).reduce(0, (s, i) => s + i * i))",
    ];
    for (i, src) in progs.iter().enumerate() {
        let (base, err, code) =
            run_source(src, &[("HELIX_THREADS", "1")], &format!("thr1_{i}"));
        assert_eq!(code, Some(0), "HELIX_THREADS=1 failed on program {i}: {err}");
        for t in ["2", "3", "6", "12"] {
            let (got, _, c) =
                run_source(src, &[("HELIX_THREADS", t)], &format!("thr{t}_{i}"));
            assert_eq!(c, Some(0), "HELIX_THREADS={t} failed on program {i}");
            assert_eq!(
                got, base,
                "HELIX_THREADS={t} changed the RESULT on program {i} — a thread count must never \
                 be observable in the output"
            );
        }
        let (dflt, _, _) = run_source(src, &[], &format!("thrd_{i}"));
        assert_eq!(dflt, base, "the default pool disagrees with HELIX_THREADS=1 on program {i}");
    }
    // Garbage, zero and negatives fall back to the default rather than erroring or hanging.
    for (i, bad) in ["0", "-4", "many", "", "3.5", "999999999999999999999"].iter().enumerate() {
        let (out, err, code) = run_source(
            "print((0..100000).map(i => i * 2).sum())",
            &[("HELIX_THREADS", bad)],
            &format!("thrbad_{i}"),
        );
        assert_eq!(code, Some(0), "HELIX_THREADS={bad:?} should be ignored, not fatal: {err}");
        assert_eq!(out.trim(), "9999900000", "HELIX_THREADS={bad:?} changed the result");
    }
}

/// An unterminated string names the delimiter it actually opened with.
///
/// With three string forms (`"…"`, `'…'`, `'''…'''`) a fixed hint would send the reader
/// hunting for the wrong character — the message is identical in all three cases, so the
/// hint is the only thing that distinguishes them.
#[test]
fn unterminated_string_hints_name_their_own_delimiter() {
    for (src, want) in [
        ("print('oops)", "add a closing `'` to end the string."),
        ("print(\"oops)", "add a closing `\"` to end the string."),
        ("print('''oops)", "add a closing `'''` to end the string."),
    ] {
        let (_, err, code) = run_source(src, &[], "unterm");
        assert_eq!(code, Some(1), "`{src}` should fail to lex");
        assert!(
            err.contains(want),
            "`{src}` did not name its own delimiter.\nwanted: {want}\ngot: {err}"
        );
    }
}

// THE extractor, not a copy of it: the same file `helix test` uses to run a user's doc
// examples. The crate has no library target, so `#[path]` is how an integration test
// shares source with the binary. Two extractors could drift, and the failure mode of a
// drifted one is silent — it finds nothing and reports success.
#[path = "../src/doctest.rs"]
mod doctest;
use doctest::{doc_examples_in, example_program};

/// **Every documented example runs, on all three engines, and must still say what it says.**
///
/// This is the difference between Helix's doc comments and Python's docstrings, and it is
/// the whole reason `##` is worth distinguishing from `#`: a docstring rots silently, and
/// `doctest` is opt-in and lives off the normal test path. Here an example that has drifted
/// fails the gate. Better still, it is checked against the tree-walker, the bytecode VM and
/// the JIT at once — so every documented example also strengthens the differential oracle,
/// which is something a single-implementation language cannot offer.
///
/// The motivation is not hypothetical. Several defects here survived because a COMMENT
/// recorded an intent the code did not implement — `numeric_cmp` claimed `total_cmp` puts
/// `NaN` "after `+inf`, as numpy does" and cited `sqrt(-1.0)`, and that example sorts to
/// the FRONT. Prose cannot be checked; an example can. See `docs/comments-and-docs.md`.
///
/// An example runs in the context of its own file, so it may call the thing it documents.
/// The file's own output is subtracted, so only what the example itself printed is compared.
#[test]
fn doc_examples_run_and_agree_on_all_three_engines() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![root.join("examples")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "helix") {
                files.push(p);
            }
        }
    }
    files.sort();

    let mut checked = 0usize;
    for path in &files {
        let src = std::fs::read_to_string(path).expect("readable example");
        let examples = doc_examples_in(&src);
        if examples.is_empty() {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap().to_str().unwrap().replace('\\', "/");

        for ex in examples {
            // The file itself, then the example. Running the file alone first gives the
            // baseline to subtract, so an example may sit in a program that prints.
            let prog = example_program(&src, &ex);
            let where_ = format!("{rel}:{}", ex.line);

            let mut rendered: Vec<(String, String)> = Vec::new();
            for (engine, env) in [
                ("tree-walker", vec![("HELIX_NOVM", "1")]),
                ("vm", vec![("HELIX_NOJIT", "1")]),
                ("jit", vec![]),
            ] {
                let (base_out, _, _) = run_source(&src, &env, &format!("docbase_{checked}"));
                let (out, err, _) = run_source(&prog, &env, &format!("docex_{checked}"));
                // What the EXAMPLE printed: whatever the file did not.
                let got = out.strip_prefix(base_out.as_str()).unwrap_or(&out).trim_end();
                let got = if got.is_empty() && !err.trim().is_empty() {
                    // An example may document an error; compare its first line.
                    err.trim().lines().next().unwrap_or("").to_string()
                } else {
                    got.to_string()
                };
                rendered.push((engine.to_string(), got));
            }
            // THE ORACLE FIRST: three engines must agree before the value means anything.
            for w in rendered.windows(2) {
                assert_eq!(
                    w[0].1, w[1].1,
                    "doc example at {where_} DIVERGES between {} and {}",
                    w[0].0, w[1].0
                );
            }
            if !ex.expect.is_empty() {
                let want = ex.expect.join("\n");
                assert_eq!(
                    rendered[0].1.trim_end(),
                    want.trim_end(),
                    "doc example at {where_} no longer produces what it documents\n  \
                     code: {}\n",
                    ex.code.join(" ; ")
                );
            }
            checked += 1;
        }
    }
    // A verifier that silently checks nothing is the failure mode this project keeps
    // finding, so refuse to pass while finding no examples at all.
    assert!(
        checked >= 3,
        "found only {checked} doc examples — the extractor or the `##` convention is broken"
    );
}

/// The `.helix` extension is optional: `helix run hello` finds `hello.helix`.
///
/// The interesting cases are the ones where it must NOT guess. Resolution APPENDS the
/// extension rather than substituting it, so `helix run notes.txt` runs `notes.txt` — it
/// never silently runs a `notes.helix` sitting beside it, which `with_extension("helix")`
/// would have done. And an exact file always wins, so an extensionless script still runs
/// and a directory cannot shadow a same-named script next to it.
#[test]
fn the_helix_extension_is_optional_but_never_guessed_over_a_real_file() {
    let dir = std::env::temp_dir().join("helix_extopt_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("adir")).unwrap();
    let w = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();

    w("hello.helix", "print(\"from-helix\")\n");
    w("plain", "print(\"from-plain\")\n");
    w("both", "print(\"exact\")\n");
    w("both.helix", "print(\"withext\")\n");
    // A non-Helix file whose stem also has a .helix beside it — the substitution trap.
    w("notes.txt", "this is not helix\n");
    w("notes.helix", "print(\"MUST-NOT-RUN\")\n");

    let at = |args: &[&str]| -> (String, String, Option<i32>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_helix"));
        cmd.current_dir(&dir).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().expect("spawn helix");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    };

    // Resolution works, with and without the extension, and via the bare shorthand.
    for args in [
        vec!["run", "hello.helix"],
        vec!["run", "hello"],
        vec!["hello"],
    ] {
        let (out, err, code) = at(&args);
        assert_eq!(code, Some(0), "`{args:?}` failed: {err}");
        assert_eq!(out.trim(), "from-helix", "`{args:?}`");
    }

    // An exact file wins over the same stem plus `.helix`.
    assert_eq!(at(&["run", "plain"]).0.trim(), "from-plain");
    assert_eq!(at(&["run", "both"]).0.trim(), "exact");
    assert_eq!(at(&["run", "both.helix"]).0.trim(), "withext");

    // THE TRAP: `notes.txt` exists, so it is what runs — it fails to parse, and
    // `notes.helix` is never executed.
    let (out, err, code) = at(&["run", "notes.txt"]);
    assert_eq!(code, Some(1), "notes.txt should fail to parse");
    assert!(!out.contains("MUST-NOT-RUN"), "ran notes.helix instead of notes.txt!");
    assert!(!err.contains("MUST-NOT-RUN"), "ran notes.helix instead of notes.txt!");
    assert!(err.contains("notes.txt"), "error should point at notes.txt: {err}");

    // Missing and directory get distinct, actionable errors.
    let (_, err, code) = at(&["run", "nope"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot read"), "{err}");
    assert!(err.contains("nope.helix"), "error should list what it looked for: {err}");

    // ...but a path that ALREADY ends in `.helix` must not be told we looked for
    // `nope.helix.helix` — that reads as the tool being confused about its own
    // filenames. Caught by an existing test when the first version did exactly that.
    let (_, err, code) = at(&["run", "nope.helix"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot read"), "{err}");
    assert!(!err.contains(".helix.helix"), "doubled extension in help: {err}");

    let (_, err, code) = at(&["run", "adir"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("is a directory"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `helix check` — type-check without running. Three properties, each of which the
/// command is useless without:
///
/// 1. **It never executes anything.** A checker with side effects is not a checker; you
///    cannot point it at a stranger's file. Pinned with a program whose only observable
///    behaviour is a `print` and a file write — after `check`, neither happened.
/// 2. **It agrees with `run` about what compiles.** `check_file_capture` is
///    `run_file_capture` minus the execution, so the same program must get the same
///    verdict and the same rendered diagnostic from both.
/// 3. **A batch does not stop at the first failure.** `scripts/checkall.sh` passes all 85
///    tracked programs at once; a gate that reported only the first broken one would need
///    85 runs to clear a tree, so every file is checked and the count is reported.
#[test]
fn check_type_checks_without_running_anything() {
    let dir = std::env::temp_dir().join("helix_check_test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let w = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();

    let sentinel = dir.join("side_effect.txt");
    w(
        "good.helix",
        &format!(
            "print(\"EXECUTED\")\n\"x\".write_to(\"{}\")\n",
            sentinel.to_str().unwrap().replace('\\', "/")
        ),
    );
    w("bad.helix", "print(undefined_name)\n");
    w("alsobad.helix", "print([1, 2].no_such_method())\n");
    // NOT a type error: immutable reassignment is enforced by the VM at run time (see
    // `immutable_reassignment_errors_on_the_vm`). `check` is a TYPE check, not a proof
    // that the program will run — pinned below so the boundary stays honest.
    w("runtime_only.helix", "x = 1\nx = 2\nprint(x)\n");

    let at = |args: &[&str]| -> (String, String, Option<i32>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_helix"));
        cmd.current_dir(&dir).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().expect("spawn helix");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.code(),
        )
    };

    // (1) A clean program checks clean — and is NOT run.
    let (out, err, code) = at(&["check", "good.helix"]);
    assert_eq!(code, Some(0), "clean program should check: {err}");
    assert!(out.contains("ok"), "expected an ok line, got {out:?}");
    assert!(!out.contains("EXECUTED"), "`check` executed the program: {out:?}");
    assert!(!sentinel.exists(), "`check` let the program write a file");
    // The extension is optional here exactly as it is for `run`.
    assert_eq!(at(&["check", "good"]).2, Some(0));

    // (2) The verdict and the diagnostic match `helix run`'s. Not "an error" — the same
    // error: a second front end that phrased things differently would drift from this one.
    for prog in ["bad.helix", "alsobad.helix"] {
        let (_, cerr, ccode) = at(&["check", prog]);
        let (rout, rerr, rcode) = at(&["run", prog]);
        assert_eq!(ccode, Some(1), "`check {prog}` should fail");
        assert_eq!(rcode, Some(1), "`run {prog}` should fail");
        assert_eq!(cerr, rerr, "`check {prog}` and `run {prog}` disagree on the diagnostic");
        assert!(rout.is_empty(), "run leaked output before the error: {rout:?}");
    }

    // THE BOUNDARY, stated rather than implied: `check` is a type check. A program that
    // only fails at run time — here, reassigning an immutable binding, which the VM
    // enforces — checks CLEAN and then fails when run. Passing `check` means "this
    // compiles", never "this works".
    assert_eq!(at(&["check", "runtime_only.helix"]).2, Some(0));
    let (_, rerr, rcode) = at(&["run", "runtime_only.helix"]);
    assert_eq!(rcode, Some(1));
    assert!(rerr.contains("immutable"), "expected the runtime error: {rerr}");

    // (3) A batch checks EVERY file and reports the count — it does not stop at the first
    // failure, and the summary is the number CI reads.
    let (out, _, code) = at(&["check", "good.helix", "bad.helix", "alsobad.helix"]);
    assert_eq!(code, Some(1), "a batch with failures must exit non-zero");
    assert_eq!(out.matches("FAIL").count(), 2, "both failures should be listed: {out:?}");
    assert!(out.contains("ok   good.helix"), "the good file should still be reported: {out:?}");
    assert!(out.contains("checked 3 files, 2 failed"), "summary: {out:?}");
    assert!(!sentinel.exists(), "batch `check` executed the program");

    // A batch that is entirely clean exits 0.
    let (out, _, code) = at(&["check", "good.helix", "good"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("checked 2 files, 0 failed"), "{out:?}");

    // Usage errors: no path, an unknown flag, a missing file.
    let (_, err, code) = at(&["check"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("needs at least one script path"), "{err}");
    let (_, err, code) = at(&["check", "--jit"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("unknown option"), "{err}");
    let (_, err, code) = at(&["check", "nope"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot read"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// v0.2.1 regression tests. Each of these was written against the BROKEN binary
// first and confirmed to FAIL there — a test that passes either way pins nothing.
// ---------------------------------------------------------------------------

/// The three engines, as `run_source` env blocks. Every fix below must hold on all
/// three: they are a differential oracle only while they agree.
const ENGINES: [(&str, &[(&str, &str)]); 3] = [
    ("jit", &[]),
    ("vm", &[("HELIX_NOJIT", "1")]),
    ("walker", &[("HELIX_NOVM", "1")]),
];

/// B1. A grouped `f64` aggregation must give the same answer every time.
///
/// The repetition is the whole test, and it must happen INSIDE ONE PROCESS. Polars
/// ran a partitioned group-by whose merge order varied per execution, so a single
/// evaluation cannot see the bug — a one-shot assertion passes against the broken
/// binary and pins nothing. Measured before the fix: `drift: 30` out of 30, five
/// runs out of five. After: `drift: 0`.
#[test]
fn grouped_float_aggregation_is_deterministic_within_one_process() {
    let src = r#"
n = 20000
g = range(0, n).map(i => i % 2)
v = range(0, n).map(i => 1.0 + (i % 97) * 0.01)
df = dataframe({g: g, v: v})
first = df.group(@g).sum(@v).sort(@g).column("v")
drift = range(0, 30).filter(i => df.group(@g).sum(@v).sort(@g).column("v") != first).count()
print("drift:", drift)
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("b1_determinism_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out.trim(), "drift: 0", "{name}: grouped sum drifted across evaluations");
    }
}

/// B2. Grouped aggregations propagate `missing`, like every other reduction (ADR 0001).
///
/// Before: `mean` answered `2.0` for a group containing an unknown, and an all-`missing`
/// group reported `sum` `0.0` — indistinguishable from a group that really sums to zero.
/// `count` is the deliberate exception: it counts ROWS, matching `[1.0, 3.0, missing].count()`
/// and `df.column("v").count()`, which both answer 3.
#[test]
fn grouped_aggregations_propagate_missing() {
    let src = r#"
d = dataframe({g: ["a", "a", "a"], v: [1.0, 3.0, missing]})
print("mean:", d.group(@g).mean(@v).column("v"))
gv = dataframe({g: ["a", "a", "b"], v: [1.0, 2.0, missing]})
print("sum:", gv.group(@g).sum(@v).sort(@g).column("v"))
print("count:", gv.group(@g).count(@v).sort(@g).column("v"))
print("array:", [1.0, 3.0, missing].count())
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("b2_missing_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "mean: [missing]\nsum: [3.0, missing]\ncount: [2, 1]\narray: 3\n",
            "{name}: grouped aggregation disagreed with the whole-column path on `missing`"
        );
    }
}

/// C2. `.sum()` of an infinity is an infinity, not `NaN`.
///
/// Neumaier compensation went non-finite once the running sum did, turning a CORRECT
/// `inf` into `NaN` — disagreeing with IEEE-754, python3, NumPy, and with Helix's own
/// `+` and `reduce`. `inf + -inf` is still `NaN`, which is correct and must stay.
/// The cancellation case guards the other direction: the compensation that makes
/// Neumaier worth having must survive the guard.
#[test]
fn sum_of_non_finite_values_matches_ieee() {
    let src = r#"
INF = exp(800.0)
print([INF].sum() == INF)
print([0.0 - INF].sum() == 0.0 - INF)
print([INF, 0.0 - INF].sum())
print([INF, 1.0].mean() == INF)
print([1.0e16, 1.0, 0.0 - 1.0e16].sum())
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("c2_inf_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        // The last line is the cancellation case: naive summation answers 0.0 here,
        // Neumaier answers the true 1.0. If that regresses, the guard ate the compensation.
        assert_eq!(out, "true\ntrue\nNaN\ntrue\n1.0\n", "{name}");
    }
}

/// C3. `erf` is computed to double precision, like every other math builtin.
///
/// It was the Abramowitz & Stegun 7.1.26 rational approximation (~1.5e-7 absolute),
/// which left a fixed pedestal near zero — so its RELATIVE error was unbounded there,
/// and the `x == 0.0` special case it needed made `erf` discontinuous at the origin.
/// The self-oracle needs no external table: `erf(x)/x` as x approaches 0 is `2/sqrt(pi)`,
/// a value Helix can compute itself. Every literal below is python3's `math.erf`.
#[test]
fn erf_is_computed_to_double_precision() {
    let src = r#"
print(erf(1.0e-12) / 1.0e-12 == 2.0 / sqrt(3.141592653589793))
print(erf(0.5))
print(erf(1.0))
print(erf(0.0), erf(0.0 - 1.0))
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("c3_erf_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "true\n0.5204998778130465\n0.8427007929497149\n0.0 -0.8427007929497149\n",
            "{name}: erf drifted from python3's math.erf"
        );
    }
}

/// D1. `try` binds tighter than binary operators, and the error now says so.
///
/// `try 1 + 1` parses as `(try 1) + 1`, so the operand really is a record — the old
/// message was true and useless, naming a Record in an expression containing none.
/// The hint keys on the AST node, not the record's shape, so a user's own
/// `{ok, value, error}` record never triggers it; the second half of this test is
/// what pins that, and it is the half that would catch an over-eager rewrite.
#[test]
fn try_binding_tighter_than_operators_is_explained() {
    for (name, env) in ENGINES {
        let (_, err, code) = run_source("r = try 1 + 1\nprint(r)\n", env, &format!("d1_try_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.contains("`try` binds tighter than `+`") && err.contains("try (a + b)"),
            "{name}: no hint naming `try`:\n{err}"
        );

        // An ORDINARY record operand must still get the ordinary message.
        let (_, plain, code) =
            run_source("r = {a: 1} + 1\nprint(r)\n", env, &format!("d1_plain_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(
            !plain.contains("binds tighter"),
            "{name}: ordinary record wrongly blamed on `try`:\n{plain}"
        );
    }
}

/// B3. An oversized materialization is a catchable Helix error, never a dead process.
///
/// ADR 0024: the runtime is total — a limit is reported, never signalled by killing the
/// host. Before this fix `range(0, 100000000).filter(...).map(i => [i]).count()` aborted
/// with SIGABRT (exit 134, core dumped) on all three engines, `try` could not catch it,
/// and nothing after it ran.
///
/// The address-space cap is what makes this cheap and deterministic to test: without it
/// the same program succeeds on a large machine after several minutes. Run through `sh`
/// so `ulimit` applies to the child, and only where that means something.
#[test]
#[cfg(unix)]
fn oversized_materialization_is_an_error_not_an_abort() {
    let src = "r = try (range(0, 100000000).filter(i => true).map(i => [i]).count())\n\
               print(\"ok:\", r.ok)\nprint(\"alive\")\n";
    let path = std::env::temp_dir().join("helix_it_b3_abort.helix");
    std::fs::write(&path, src).unwrap();

    for (name, extra) in [
        ("jit", ""),
        ("vm", "export HELIX_NOJIT=1; "),
        ("walker", "export HELIX_NOVM=1; "),
    ] {
        // The engine switch is EXPORTED before the `exec`: `exec VAR=1 cmd` is not an
        // assignment in sh, it looks for a command literally named `VAR=1`.
        let script = format!(
            "ulimit -v 3670016; {}exec {} {} < /dev/null",
            extra,
            env!("CARGO_BIN_EXE_helix"),
            path.display()
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to spawn sh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        // The process must survive: SIGABRT shows up as a None exit code (signalled).
        assert_eq!(
            out.status.code(),
            Some(0),
            "{name}: process died instead of reporting a limit\n{stderr}"
        );
        assert!(
            stdout.contains("ok: false") && stdout.contains("alive"),
            "{name}: `try` did not catch the limit, or execution stopped:\n{stdout}\n{stderr}"
        );
        assert!(
            !stderr.contains("memory allocation of"),
            "{name}: raw allocator abort leaked to the user:\n{stderr}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// C1. IUPAC ambiguity codes participate in `gc_content`/`at_content` by what they
/// actually assert, not by accident of spelling.
///
/// Before: `S` ("G or C" — GC by definition) was counted as non-GC while staying in
/// the denominator, arithmetically identical to declaring it an A or a T, so
/// `dna("GCS")` read LOWER than the same sequence without the S and `dna("S")` read
/// 0.0 where 1.0 is provable. The policy pinned here: S is GC, W is not, and the codes
/// genuinely ambiguous about GC-ness (N, R Y K M B D H V) are excluded from numerator
/// and denominator alike — extending the rule N always had. A sequence with NO
/// classifiable base has an unknown fraction: `missing` (ADR 0001), because 0.0 there
/// is indistinguishable from a genuinely AT-only answer — and `missing` then
/// propagates through `mean_gc`, which used to average an all-N sequence in as 0.0.
#[test]
fn dna_iupac_arithmetic_is_correct() {
    let src = r#"
print(dna("S").gc_content(), dna("S").at_content())
print(dna("GCS").gc_content())
print(dna("GCR").gc_content())
print(dna("GCN").gc_content())
print(dna("NNN").gc_content())
print(dna("RRRR").at_content())
print(dna("ATGCGC").gc_content())
print([dna("GC"), dna("NN")].mean_gc())
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("c1_iupac_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "1.0 0.0\n1.0\n1.0\n1.0\nmissing\nmissing\n0.6666666666666666\nmissing\n",
            "{name}: IUPAC GC arithmetic drifted from the documented policy"
        );
    }
}

/// `unique` on a packed array must not box the whole buffer just to probe a set.
///
/// The general method dispatch expands a packed array via `to_values()` before the
/// method runs; for `unique` that meant 80M packed ints became ~1.9 GB of boxed
/// `Value`s — and an allocator ABORT under a memory cap — before a single comparison.
/// (`zip`/`enumerate` were hoisted above that expansion long ago for the same reason;
/// `unique` never got the treatment.) The packed fast path probes raw scalars, so the
/// same program now finishes in a 1000-entry set on the JIT and refuses cleanly (ADR
/// 0024) where memory genuinely does not suffice. Semantics are pinned unchanged by
/// the second program: NaN survives (it belongs to no equivalence class), ±0.0
/// collapse to first-seen, first-seen order throughout.
#[test]
#[cfg(unix)]
fn unique_on_a_packed_array_does_not_abort() {
    let src = "print(\"n:\", try (range(0, 80000000).map(i => i % 1000).unique().count()))\n\
               print(\"alive\")\n";
    let path = std::env::temp_dir().join("helix_it_unique_abort.helix");
    std::fs::write(&path, src).unwrap();
    for (name, extra) in [
        ("jit", ""),
        ("vm", "export HELIX_NOJIT=1; "),
        ("walker", "export HELIX_NOVM=1; "),
    ] {
        let script = format!(
            "ulimit -v 3670016; {}exec {} {} < /dev/null",
            extra,
            env!("CARGO_BIN_EXE_helix"),
            path.display()
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("failed to spawn sh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{name}: process died\n{stderr}");
        assert!(stdout.contains("alive"), "{name}: execution stopped\n{stdout}\n{stderr}");
        assert!(
            !stderr.contains("memory allocation of"),
            "{name}: raw allocator abort leaked\n{stderr}"
        );
    }
    let _ = std::fs::remove_file(&path);

    // The packed fast path must reproduce the general path's classes exactly.
    let sem = r#"
INF = exp(800.0)
nan = INF - INF
print([1.0, nan, nan, 1.0, 0.0 - 0.0, 0.0].unique().count())
print(range(0, 10).map(i => i % 3).unique())
print(range(0, 5).unique())
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(sem, env, &format!("unique_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "4\n[0, 1, 2]\n[0, 1, 2, 3, 4]\n", "{name}: unique classes drifted");
    }
}

/// The i64 map kernel admits AFFINE indices (`a[2*i]`, `a[i+1]`, `a[i*k+3]`) — previously
/// only the bare counter (`a[i]`) and a free scalar (`a[k]`) were eligible, so an affine
/// body fell to the per-element bytecode loop: measured 486 -> 27 ms at 10M elements, and
/// the 3-point stencil `a[i] + a[i+1] + a[i+2]` 1871 -> 31 ms at 20M, against the PGO
/// release binary. (The reduce path deliberately still declines affine bounds: admitting
/// them there changed which compiled form the body got without the dispatch taking a
/// kernel — see the site comment in `bytecode/comprehensions.rs`.)
///
/// What this pins is CORRECTNESS, not speed: the kernel does unchecked native loads, so
/// the values — and the out-of-bounds/negative-index behaviour, which must decline to the
/// exact interpreter semantics (Python-wrap for negatives, the precise OOB error text) —
/// must be bit-identical on all three engines. The `let` shadowing case guards the
/// soundness rule that a rebound base/coef name refuses the kernel rather than running a
/// stale bounds proof.
#[test]
fn affine_indexed_map_is_correct_on_all_engines() {
    let src = r#"
n = 1000
a = range(0, n).map(i => i * 3 + 7)
print(range(0, 500).map(i => a[2 * i]).sum())
print(range(0, 999).map(i => a[i + 1]).sum())
print(range(0, 500).map(i => a[2 * i + 1] - a[2 * i]).sum())
k = 5
print(range(0, 100).map(i => a[i * k + 3]).sum())
b = range(0, 10).map(i => i * 10)
r = try (range(0, 7).map(i => b[2 * i]).sum())
print(r.ok, r.error)
print(range(0, 5).map(i => b[i - 5]).sum())
"#;
    let want = "752000\n1505493\n1500\n75850\nfalse index 10 is out of bounds for length 10\n350\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("affine_map_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: affine-indexed map drifted");
    }
}

// ---------------------------------------------------------------------------
// DX hardening (docs/dx-plan.md do-now items). Each behavioral test was run
// against the v0.2.1 binary first and confirmed to fail there.
// ---------------------------------------------------------------------------

/// The REPL banner carries the map (dx-plan 1). Bare `helix` is where a new user or
/// agent lands first; the project's own history prices the missing pointer at months
/// of designing around a "missing" `scan` that `helix doc Array` printed all along.
#[test]
fn repl_banner_points_at_help_doc_and_describe() {
    let (out, _, code) = run(&[], &[], "");
    assert_eq!(code, Some(0));
    for needle in ["helix help", "helix doc [Type]", "helix describe"] {
        assert!(out.contains(needle), "banner lost `{needle}`:\n{out}");
    }
}

/// `helix test <file>` on a doc module answers what `helix test <dir>` answers
/// (dx-plan 3). Before: the same command that PASSED a module's examples also FAILed
/// the file for asserting nothing, in one output — an agent narrowing from directory
/// to file to iterate faster was punished for it. A `*_test.helix` named directly
/// keeps the assert-or-fail contract.
#[test]
fn helix_test_on_a_doc_module_file_matches_the_directory_run() {
    let dir = std::env::temp_dir().join("helix_it_docmod");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("mylib.helix");
    std::fs::write(
        &module,
        "## Doubles a number.\n##\n##     >>> double(21)\n##     42\nexport fn double(x) = x * 2\n",
    )
    .unwrap();

    let (dir_out, _, dir_code) = run(&["test", dir.to_str().unwrap()], &[], "");
    let (file_out, _, file_code) = run(&["test", module.to_str().unwrap()], &[], "");
    assert_eq!(dir_code, Some(0), "dir run failed:\n{dir_out}");
    assert_eq!(file_code, Some(0), "file run must pass like the dir run:\n{file_out}");
    assert!(
        !file_out.contains("without asserting anything"),
        "file run still demands assertions from a doc module:\n{file_out}"
    );
    assert!(file_out.contains("1 passed"), "examples did not run:\n{file_out}");

    // The pathological case keeps its contract: an assertion-free *_test.helix
    // named directly still fails, exactly as the directory run would fail it.
    let tfile = dir.join("empty_test.helix");
    std::fs::write(&tfile, "x = 1\n").unwrap();
    let (t_out, _, t_code) = run(&["test", tfile.to_str().unwrap()], &[], "");
    assert_eq!(t_code, Some(1), "assertion-free test file must still FAIL:\n{t_out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `expect(k)` — the loud lookup (dx-plan 4). `get`/`d[k]` keep ADR 0001's propagating
/// `missing`; `expect` raises at the miss, BEFORE a `missing` is minted and laundered
/// through arithmetic into a number-shaped hole. Pins: hit, miss with a one-edit
/// did-you-mean, miss with the fallback hint, and all three engines byte-identical.
#[test]
fn expect_is_the_raising_lookup_on_dict_and_record() {
    let src = r#"
d = [("alpha", 1), ("beta", 2)].to_dict()
print(d.expect("alpha"))
r = try d.expect("alpah")
print(r.ok, "|", r.error)
rec = {name: "x", size: 3}
print(rec.expect("size"))
q = try rec.expect("nmae")
print(q.ok, "|", q.error)
"#;
    let want = "1\nfalse | key `alpah` not found in this dict (2 keys)\n3\nfalse | field `nmae` not found in this record (2 fields)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("expect_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }
    // The rendered hints: a one-edit typo suggests the real key; a far miss teaches
    // the quiet alternatives instead of guessing.
    let (_, err, code) = run_source(
        "d = [(\"alpha\", 1)].to_dict()\nprint(d.expect(\"alpah\"))\n",
        &[],
        "expect_hint_near",
    );
    assert_eq!(code, Some(1));
    assert!(err.contains("did you mean `alpha`?"), "{err}");
    let (_, err, code) =
        run_source("rec = {name: 1}\nprint(rec.expect(\"zz\"))\n", &[], "expect_hint_far");
    assert_eq!(code, Some(1));
    assert!(err.contains("`.has(k)` checks presence"), "{err}");
}

/// `helix doc <name>` answers the question users actually arrive with (dx-plan 5):
/// "is there a scan, and how do I call it?" — owners, effect, an example receiver.
/// Multi-owner names report every owner; unknown names keep the unknown-type error.
#[test]
fn helix_doc_reverse_looks_up_methods_and_builtins() {
    let (out, _, code) = run(&["doc", "scan"], &[], "");
    assert_eq!(code, Some(0));
    assert!(out.contains("`scan` is a method on Array") && out.contains("helix doc Array"), "{out}");

    let (out, _, code) = run(&["doc", "sqrt"], &[], "");
    assert_eq!(code, Some(0));
    assert!(out.contains("`sqrt` is a free function"), "{out}");

    let (out, _, code) = run(&["doc", "max"], &[], "");
    assert_eq!(code, Some(0));
    for ty in ["Array", "Tensor", "GroupBy"] {
        assert!(out.contains(&format!("is a method on {ty}")), "missing owner {ty}:\n{out}");
    }

    let (_, err, code) = run(&["doc", "zzz"], &[], "");
    assert_eq!(code, Some(1));
    assert!(err.contains("unknown type `zzz`"), "{err}");
}

/// Three parse-help gaps (dx-plan 6). (a) a foreign-idiom running sum steers to
/// `cumsum`/`scan` instead of dumping 79 names, and the no-near-miss fallback points at
/// `helix doc <Type>`; (b) an undefined bare name inside a string hole teaches that
/// braces are interpolation and shows the `{{ }}` escape; (f) `fn` inside `do { }`
/// names the item-level rule and the lambda form. (a)'s runtime twin must be
/// byte-identical on all three engines.
#[test]
fn parse_help_gaps_are_closed() {
    for (name, env) in ENGINES {
        let (_, err, code) =
            run_source("print([1, 2].prefix_sum())\n", env, &format!("prefix_sum_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains("a prefix sum is `xs.cumsum()`"), "{name}: {err}");

        let (_, err, code) =
            run_source("print([1, 2].zzyzx())\n", env, &format!("nomethod_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.contains("no similar method — `helix doc Array` lists all Array methods."),
            "{name}: {err}"
        );
    }
    let (_, err, code) = run_source("print(\"value: {feat}\")\n", &[], "interp_hole");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("inside a string is interpolation") && err.contains("{{feat}}"),
        "{err}"
    );
    let (_, err, code) = run_source("y = do { fn f(x) = x\n  1 }\nprint(y)\n", &[], "fn_in_do");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("`fn` cannot be defined inside a `do` block")
            && err.contains("f = (x) => x * 2"),
        "{err}"
    );
}

/// The string/record fold is linear (dx-plan 7): `concat_in_place` gained a `Values`
/// arm guarded by a non-numeric witness, which makes repacking impossible by
/// construction — so what is pinned is VALUE and REPRESENTATION equality between the
/// fold spelling and plain `concat`, on all three engines. (Timing is not asserted:
/// wall clock here is ±15%; the measured change was 235.8s -> 61ms at 256k pieces.)
#[test]
fn string_fold_matches_plain_concat_in_value_and_representation() {
    let src = r#"
parts = range(0, 2000).map(i => "line{i}")
a = parts.reduce([], (acc, s) => acc.concat([s]))
b = [].concat(parts)
print(a == b, a.count(), a[0], a[1999])
mixed = range(0, 100).map(i => i).concat(["x"])
c = mixed.reduce([], (acc, v) => acc.concat([v]))
d = [].concat(mixed)
print(c == d, c.count(), c[100])
nums = range(0, 50).reduce([], (acc, i) => acc.concat([i]))
print(nums == range(0, 50).map(i => i), nums.sum())
"#;
    let want = "true 2000 line0 line1999\ntrue 101 x\ntrue 1225\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("strfold_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: fold diverged from plain concat");
    }
}

/// `helix describe` carries signatures derived from the checker's own tables (dx-plan
/// do-later, landed): accepted arities plus per-arity return types, `null` where the
/// checker genuinely does not constrain or determine them — never fabricated. `round`
/// is the shape that forces per-arity returns (1 → Int, 2 → Float); `random` is the
/// honest-null shape (unguarded arity, arity-independent `Array<Float>` return).
#[test]
fn describe_reports_signatures_from_the_checker() {
    let (out, _, code) = run(&["describe"], &[], "");
    assert_eq!(code, Some(0));
    let doc: serde_json::Value = serde_json::from_str(&out).expect("describe is JSON");
    let builtins = doc["builtins"].as_array().unwrap();
    let by_name = |n: &str| {
        builtins
            .iter()
            .find(|b| b["name"] == n)
            .unwrap_or_else(|| panic!("`{n}` missing from describe"))
    };

    let sqrt = by_name("sqrt");
    assert_eq!(sqrt["signatures"], serde_json::json!([{"args": 1, "returns": "Float"}]));

    let round = by_name("round");
    assert_eq!(
        round["signatures"],
        serde_json::json!([
            {"args": 1, "returns": "Int"},
            {"args": 2, "returns": "Float"}
        ])
    );

    let random = by_name("random");
    assert!(random["signatures"].is_null(), "unguarded arity must be null, not invented");
    assert_eq!(random["returns"], "Array<Float>");

    // Every entry is one of the two honest states: a nonempty signature list, or null.
    for b in builtins {
        let sigs = &b["signatures"];
        assert!(
            sigs.is_null() || sigs.as_array().is_some_and(|a| !a.is_empty()),
            "`{}` has an empty signature list — the probe found no accepted arity",
            b["name"]
        );
    }
}

/// The missing-data escape hatch in queries (dx-plan do-later, landed; the last open
/// edge of the v0.2.1 B2 blocker). `where(@v == missing)` selects nothing and always
/// will — `missing == missing` is `missing` under ADR 0001, and queries deliberately
/// agree with arrays — so intent needs its own spellings: `@col.is_missing()` inside
/// a predicate (lowered to the Arrow validity bitmap, which IS Helix's `missing`),
/// its `not` negation, and frame-level `drop_missing()`, which keeps rows where EVERY
/// column is non-missing and desugars to the same validated filter `where` runs.
#[test]
fn queries_can_name_missingness_explicitly() {
    let src = r#"
df = dataframe({g: ["a", "b", "c", "d"], v: [1.0, missing, 3.0, missing], w: [10, 20, missing, 40]})
print(df.where(@v.is_missing()).column("g"))
print(df.where(not @v.is_missing()).count())
print(df.drop_missing().column("g"))
print(df.where(@v == missing).count())
r = try df.drop_missing(@v)
print(r.ok)
"#;
    let want = "[\"b\", \"d\"]\n2\n[\"a\"]\n0\nfalse\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("missing_query_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: missingness spellings drifted");
    }
}

/// `helix.toml` is a real manifest: description/authors/license/repository/keywords are
/// accepted, `version` must be comparable MAJOR.MINOR.PATCH, and a declared `helix`
/// toolchain floor is ENFORCED at manifest load — the #19 review's incident verbatim:
/// an old binary on new syntax must say "your binary is too old" once, not fail with
/// sixty confusing parse errors. Enforcement lives in `Manifest::load`, the one seam
/// `run`/`test`/`sync` and every dependency manifest already go through.
#[test]
fn helix_toml_carries_metadata_and_enforces_the_toolchain_floor() {
    let dir = std::env::temp_dir().join("helix_it_manifest");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let toml = dir.join("helix.toml");
    let prog = dir.join("m.helix");
    std::fs::write(&prog, "print(1 + 1)\n").unwrap();
    let run = |manifest: &str| {
        std::fs::write(&toml, manifest).unwrap();
        run(&[prog.to_str().unwrap()], &[], "")
    };

    // The full metadata surface parses, and a satisfied floor runs.
    let (out, err, code) = run(
        "[package]\nname = \"physics\"\nversion = \"0.1.0\"\n\
         description = \"Orbital mechanics\"\nauthors = [\"A <a@b.c>\"]\n\
         license = \"MIT\"\nkeywords = [\"physics\"]\nhelix = \">=0.1.0\"\n[dependencies]\n",
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "2\n");

    // An unsatisfiable floor is ONE clear error naming both versions.
    let (_, err, code) = run(
        "[package]\nname = \"physics\"\nhelix = \">=9.0.0\"\n[dependencies]\n",
    );
    assert_eq!(code, Some(1));
    assert!(
        err.contains("requires Helix >= 9.0.0") && err.contains("this binary is"),
        "{err}"
    );

    // A version that cannot be compared is not a version.
    let (_, err, code) =
        run("[package]\nname = \"physics\"\nversion = \"1.0\"\n[dependencies]\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("must be MAJOR.MINOR.PATCH"), "{err}");

    // A malformed floor names the accepted forms.
    let (_, err, code) =
        run("[package]\nname = \"physics\"\nhelix = \"banana\"\n[dependencies]\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("must be a minimum version"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reassigning a seeded math constant names the constant and warns about shadowing —
/// the generic hint was actively harmful there: "declare it as mutable up front" WORKS
/// for `e`, silently shadowing Euler's number for the whole file (the natural variable
/// name for elementary charge, which is how a physics library found this). An ordinary
/// immutable binding keeps the generic wording. Byte-identical on all three engines:
/// the message pair is built by one shared helper.
#[test]
fn seeded_constants_explain_themselves_on_reassignment() {
    for (name, env) in ENGINES {
        let (_, err, code) = run_source("e = 3\nprint(e)\n", env, &format!("econst_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.contains("`e` is a built-in constant (Euler's number, 2.71828...)")
                && err.contains("would shadow the constant"),
            "{name}: {err}"
        );

        let (_, err, code) =
            run_source("x = 1\nx = 2\nprint(x)\n", env, &format!("ximm_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.contains("`x` is immutable and cannot be reassigned")
                && err.contains("mut x = ..."),
            "{name}: ordinary binding lost the generic wording: {err}"
        );
    }
}

/// `helix test a.helix b.helix` runs EVERY named file (physics-library field report).
/// It used to take the first path and silently drop the rest while printing "running
/// 1 test file" — anyone verifying two modules in one command believed both passed
/// when only the first ran. The second file here asserts 4 == 5: if it doesn't run,
/// this test's rc assertion is the alarm.
#[test]
fn helix_test_runs_every_named_file() {
    let dir = std::env::temp_dir().join("helix_it_multitest");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("a_test.helix");
    let b = dir.join("b_test.helix");
    std::fs::write(&a, "assert_eq(1 + 1, 2)\n").unwrap();
    std::fs::write(&b, "assert_eq(2 + 2, 5)\n").unwrap();
    let (out, _, code) = run(&["test", a.to_str().unwrap(), b.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "the failing second file must be RUN:\n{out}");
    assert!(out.contains("running 2 test files"), "{out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A method on an imported namespace is a qualified module call, never comprehension
/// sugar (physics-library field report). Seven receiver-blind parse-time desugars —
/// position, sort_by, take_while, drop_while, min_by, max_by, zipmap — intercepted
/// `mechanics.position(x0, v, a, t)` and rejected the module's own 4-arg export with
/// "takes one predicate function". The parser now tracks import namespaces and leaves
/// their method calls alone. Array sugar on real arrays is pinned unchanged.
#[test]
fn qualified_module_calls_win_over_comprehension_sugar() {
    let dir = std::env::temp_dir().join("helix_it_modsugar");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mechanics.helix"),
        "export fn position(x0, v, a, t) = x0 + v * t + 0.5 * a * t * t\n\
         export fn take_while(a, b) = a * 10 + b\n\
         export fn zipmap(x) = x + 1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.helix"),
        "import mechanics\n\
         print(mechanics.position(0.0, 10.0, 0.0, 2.0))\n\
         print(mechanics.take_while(3, 4))\n\
         print(mechanics.zipmap(41))\n\
         print([5, 6, 7].position(it == 6))\n\
         print([1, 2, 3, 0].take_while(it > 0))\n",
    )
    .unwrap();
    let want = "20.0\n34\n42\n1\n[1, 2, 3]\n";
    for (name, env) in ENGINES {
        let mut envv: Vec<(&str, &str)> = env.to_vec();
        envv.push(("HELIX_PATH", dir.to_str().unwrap()));
        let (out, err, code) = run(&["run", dir.join("main.helix").to_str().unwrap()], &envv, "");
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: qualified call or array sugar drifted");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Duplicate fold binders `(a, a)` never take the accumulator fast path (ADR 0029
/// plan, fix #0). They are legal, last-write-wins — `a` in the body is the ELEMENT —
/// but `emit_reduce_body_and_store` matched the fast path's receiver by NAME against
/// the accumulator binder, so the VM and JIT emitted `ConcatIntoLocal` folding into
/// the ACCUMULATOR: `[9, 9]` where the walker answered `[2, 9]`, silent, exit 0 — a
/// live three-engine divergence on released v0.2.1, found by the ADR 0029 design
/// recon. The guard declines the sugar for `pa == pb`; the scalar fold and the
/// ordinary distinct-binder fold are pinned unchanged.
#[test]
fn duplicate_fold_binders_agree_on_all_engines() {
    let src = r#"
print([[1], [2]].reduce([], (a, a) => a.concat([9])))
d1 = [("k", 7)].to_dict()
d2 = [("j", 1)].to_dict()
print([d1, d2].reduce(dict(), (a, a) => a.insert("n", a.count())))
print(range(0, 3).reduce(0, (a, a) => a + a))
print([[5], [6]].reduce([], (acc, x) => acc.concat(x)))
"#;
    let want = "[2, 9]\n{\"j\" => 1, \"n\" => 1}\n4\n[5, 6]\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("dup_binders_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: duplicate-binder fold diverged");
    }
}

/// The module-namespace guard reaches interpolation holes (physics field report,
/// v0.2.2). The v0.2.2 fix covered `print(mod.position(…))` but not
/// `emit("{mod.position(…)}")` — an interpolation hole is parsed by a FRESH Parser,
/// and its empty `imports` set meant the comprehension desugars fired again exactly
/// where users print things. `imports` now rides into `parse_expression` the same way
/// `fn_sigs` already did. Array sugar inside holes is pinned unchanged.
#[test]
fn qualified_module_calls_work_inside_interpolation() {
    let dir = std::env::temp_dir().join("helix_it_interpmod");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("modx.helix"),
        "export fn position(a, b, c, d) = a + b + c + d\nexport fn zipmap(x) = x + 1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.helix"),
        "import modx\n\
         print(\"{modx.position(1.0, 2.0, 3.0, 4.0)}\")\n\
         print(\"val: {modx.zipmap(41)}\")\n\
         print(\"{[5, 6, 7].position(it == 6)}\")\n",
    )
    .unwrap();
    let want = "10.0\nval: 42\n1\n";
    for (name, env) in ENGINES {
        let mut envv: Vec<(&str, &str)> = env.to_vec();
        envv.push(("HELIX_PATH", dir.to_str().unwrap()));
        let (out, err, code) = run(&["run", dir.join("main.helix").to_str().unwrap()], &envv, "");
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: interpolation-hole module call drifted");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The walker's fold is amortized-linear (ADR 0029, plan #1) — the last engine where
/// an accumulator rebuild was quadratic. The VM's take-append-store discipline is
/// transplanted: the exact `acc.concat(e)` / `acc.insert(k, v)` shapes take the
/// binding's value out before appending, so an unaliased accumulator extends in place;
/// everything else — and every aliased case — keeps the copy path with identical
/// answers. Measured at landing: 262k appends 6,768 → 23 ms; 64k dict inserts
/// 17,689 → 16 ms.
///
/// The pin is the COMPLEXITY CLASS, not wall-clock (wall clock here is ±15%): time
/// n and 4n in one process pair and assert the ratio stays far below quadratic's
/// ~16×. Threshold 8× leaves headroom for startup constants and load while a
/// quadratic regression (28.8× measured before the fix) still fails loudly.
#[test]
fn walker_fold_append_is_linear() {
    let time_of = |n: usize| {
        let src = format!(
            "print(range(0, {n}).reduce([], (acc, i) => acc.concat([i])).count())\n"
        );
        let start = std::time::Instant::now();
        let (out, err, code) =
            run_source(&src, &[("HELIX_NOVM", "1")], &format!("wfold_lin_{n}"));
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out.trim(), n.to_string());
        start.elapsed().as_secs_f64()
    };
    let t1 = time_of(65_536);
    let t4 = time_of(262_144);
    let ratio = t4 / t1.max(1e-9);
    assert!(
        ratio < 8.0,
        "walker fold went quadratic again: 4x n cost {ratio:.1}x (t1={t1:.3}s t4={t4:.3}s)"
    );
}

/// The fold fast path's semantics, pinned on all three engines: a self-referencing
/// argument sees the live accumulator (and declines in-place via the Rc), a shared
/// init survives the fold untouched, both runtime error texts match the general
/// path's words, scan keeps snapshot history, and a mid-fold error restores the
/// shadowed outer binding (the p4/p5 contract, previously pinned nowhere).
#[test]
fn walker_fold_fast_path_semantics_are_pinned() {
    let src = r#"
print(range(0, 5).reduce([], (acc, i) => acc.concat([acc.count()])))
pre = [1, 2]
print([3, 4].reduce(pre, (acc, x) => acc.concat([x])))
print(pre)
z = [[9], 0]
r = try [1, 2].reduce(z[1], (acc, x) => acc.concat([x]))
print(r.ok, "|", r.error)
q = try [[1]].reduce([], (acc, x) => acc.concat(x[0]))
print(q.ok, "|", q.error)
s = [1, 2, 3].scan([], (acc, x) => acc.concat([x]))
print(s.count(), s[2])
acc = 99
w = try range(0, 6).reduce([], (acc, i) => if i == 3 then acc.concat([1 // 0]) else acc.concat([i]))
print(w.ok, acc)
"#;
    let want = "[0, 1, 2, 3, 4]\n[1, 2, 3, 4]\n[1, 2]\nfalse | an Int has no method `concat`\nfalse | `concat` expects arrays, but argument 1 is an Int\n3 [1, 2, 3]\nfalse 99\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("wfold_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: fold fast-path semantics drifted");
    }
}

/// The string-interpolation fold is amortized-linear on every engine (ADR 0029, plan
/// #2): `Op::AppendStrIntoLocal` on the VM (which also serves JIT mode — a Str-init
/// fold never takes a kernel), and the walker's `fold_append_str` twin. Before, every
/// element copied the whole accumulator into a fresh `String` (13.6–14.6× per 4× n on
/// all three engines); after, 4× n costs ~1.8×. Same class pin as the walker fold:
/// ratio, not wall-clock, threshold 8× against quadratic's ~14×.
#[test]
fn interpolation_fold_is_linear_on_all_engines() {
    for (name, env) in ENGINES {
        let time_of = |n: usize| {
            let src = format!(
                "print(range(0, {n}).reduce(\"\", (acc, x) => \"{{acc}}x\").length())\n"
            );
            let start = std::time::Instant::now();
            let (out, err, code) = run_source(&src, env, &format!("strfold_{name}_{n}"));
            assert_eq!(code, Some(0), "{name}: {err}");
            assert_eq!(out.trim(), n.to_string(), "{name}");
            start.elapsed().as_secs_f64()
        };
        let t1 = time_of(65_536);
        let t4 = time_of(262_144);
        let ratio = t4 / t1.max(1e-9);
        assert!(
            ratio < 8.0,
            "{name}: interp fold went quadratic again: 4x n cost {ratio:.1}x \
             (t1={t1:.3}s t4={t4:.3}s)"
        );
    }
}

/// The string fold fast path's semantics, pinned on all three engines against the
/// general path's behavior: a non-`Str` init FORMATS (never errors) and re-engages the
/// fast path next iteration; a format spec on the accumulator hole DECLINES (it
/// re-pads the whole accumulator — append would be wrong); a shared init survives
/// untouched; specs on element holes work inside the fast path; scan keeps snapshot
/// history; a mid-fold error is caught. Every line byte-identical to the pre-change
/// released binary at landing.
#[test]
fn string_fold_fast_path_semantics_are_pinned() {
    let src = r#"
print(range(0, 3).reduce("", (acc, x) => "{acc}{x}"))
print(range(0, 3).reduce(0, (acc, x) => "{acc}{x}"))
print(range(0, 3).reduce("", (acc, x) => "{acc:>4}{x}"))
s0 = "seed"
print(range(0, 3).reduce(s0, (acc, x) => "{acc}{x}"), s0)
print(range(0, 3).reduce("", (acc, x) => "{acc}{x}!{x * 2}"))
print(range(0, 4).scan("", (acc, x) => "{acc}{x}"))
print(range(0, 3).reduce("", (a, x) => "{a}{x:03d}"))
r = try range(0, 3).reduce("", (acc, x) => "{acc}{1 // 0}")
print(r.ok)
"#;
    let want = "012\n0012\n    012\nseed012 seed\n0!01!22!4\n[\"0\", \"01\", \"012\", \"0123\"]\n000001002\nfalse\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("strfold_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: string-fold fast path drifted");
    }
}

/// The autodiff surface gaps the nn field report mapped (docs/dx-plan.md): `sin`/`cos`/
/// `abs` now have derivative arms on the tape (abs takes the same subgradient
/// convention at its kink that relu already uses), `.sum()` on an array of tracked
/// values folds on the tape — before, the two spellings of one sum silently FORKED BY
/// CAPABILITY (the reduce fold differentiated while `.sum()` errored, ADR 0003's
/// wound) — and `to_array` crosses the tensor/tape wall natively: it was Python-gated,
/// stranding the BLAS path's results on a stock binary. Values pinned against the
/// analytic derivatives, on all three engines.
#[test]
fn autodiff_surface_covers_the_nn_report_gaps() {
    let src = r#"
x = variable(1.5)
print(gradient(sin(x), x) == cos(1.5))
y = variable(1.5)
print(gradient(cos(y), y) == 0.0 - sin(1.5))
z = variable(0.0 - 2.0)
print(gradient(abs(z), z))
a = variable(2.0)
b = variable(5.0)
s = [a * a, b, a].sum()
print(value_of(s), gradient(s, a), gradient(s, b))
t = tensor([[1.0, 2.0], [3.0, 4.0]])
print(to_array(t), to_array(t).sum())
print(to_array(t.matmul(t)))
"#;
    let want = "true\ntrue\n-1.0\n11.0 5.0 1.0\n[1.0, 2.0, 3.0, 4.0] 10.0\n[7.0, 10.0, 15.0, 22.0]\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("nn_gaps_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: autodiff/tensor surface drifted");
    }
}

/// A reduce's init no longer needs to be a literal to take the f64 kernel (the llm
/// field report's 21–53× cliff: identical body, identical answer, `reduce(1.0, …)`
/// 59 ms vs `reduce(a0, …)` 3,117 ms at 100M — the natural ODE-integrator spelling).
/// The literal-match in four gates was a static type oracle standing in for a runtime
/// check the dispatch ALREADY makes (`Value::Float` confirms the f64 ABI, anything
/// else falls back) — so a non-literal init now enters the float family and the
/// runtime decides. The class pin runs the same fold with a literal and a parameter
/// init in one process and bounds their ratio: pre-fix it was >20×, kernel-vs-kernel
/// it is ~1×; 8× leaves noise headroom while the cliff still fails loudly.
#[test]
fn reduce_init_may_be_a_parameter_and_still_jit() {
    let time_of = |src: &str, tag: &str| {
        let start = std::time::Instant::now();
        let (out, err, code) = run_source(src, &[], tag);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out.trim(), "1.0100501679192893");
        start.elapsed().as_secs_f64()
    };
    let lit = time_of(
        "fn go() = range(0, 100000000).reduce(1.0, (a, i) => a * 1.0000000001)\nprint(go())\n",
        "init_lit",
    );
    let par = time_of(
        "fn go(a0) = range(0, 100000000).reduce(a0, (a, i) => a * 1.0000000001)\nprint(go(1.0))\n",
        "init_par",
    );
    let ratio = par / lit.max(1e-9);
    assert!(
        ratio < 8.0,
        "the reduce-init cliff is back: parameter init cost {ratio:.1}x the literal \
         (lit={lit:.3}s par={par:.3}s)"
    );

    // Semantics across the family, all three engines: a parameter init, an
    // Int-at-runtime init (dispatch declines the f64 kernel, the bytecode loop
    // answers identically), a captured dot-product with a parameter init, and a
    // mut-global init.
    let sem = r#"
fn scale(a0) = range(0, 1000).reduce(a0, (a, i) => a * 1.001)
print(scale(2.0) == 2.0 * scale(1.0))
print(scale(3))
fn dot(x, y, s0) = range(0, x.count()).reduce(s0, (a, j) => a + x[j] * y[j])
xs = range(0, 1000).map(i => i + 0.5)
ys = range(0, 1000).map(i => 2.0)
print(dot(xs, ys, 0.0), dot(xs, ys, 100.0) - dot(xs, ys, 0.0))
"#;
    let want = "true\n8.150771796706774\n1000000.0 100.0\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(sem, env, &format!("init_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: parameter-init fold semantics drifted");
    }
}

/// `let` inside a float reduce body takes the JIT kernel (the field's ~19-23× trap,
/// the last member of the reduce-eligibility family). The three f64 analyses carry a
/// scoped `locals` map — a binding is typed by its init and visible to the bindings
/// after it, rebinding the accumulator or counter declines, and an index mentioning a
/// local declines (bounds pre-evaluate in the enclosing scope) — and `gen_f64_typed`
/// gained a save/restore `Let` arm over a now-mutable binder map, the walker's own
/// scope choreography. The class pin follows the field's hard-won methodology: a
/// NOJIT control column, because their first probe used `%` which blocked BOTH arms
/// and read as a 1.0× false "fixed". Pre-fix the JIT/NOJIT ratio on this shape was
/// ~1.0 (never compiled); with the kernel engaged it is >30×; threshold 4× fails
/// loudly either way without flaking.
#[test]
fn let_in_float_reduce_body_takes_the_kernel() {
    let src = "n = 50000\n\
               xs = range(0, n).map(i => (i % 97) * 0.25)\n\
               t = range(0, 64).map(i => i * 0.5)\n\
               print(range(0, n - 64).reduce(0.0, (m, s) =>\n\
                 m + range(0, 64).reduce(0.0, (acc, j) => let d = xs[s + j] - t[j] in acc + d * d)))\n";
    let time_with = |env: &[(&str, &str)], tag: &str| {
        let start = std::time::Instant::now();
        let (out, err, code) = run_source(src, env, tag);
        assert_eq!(code, Some(0), "{err}");
        (start.elapsed().as_secs_f64(), out)
    };
    let (jit, out_j) = time_with(&[], "let_kernel_jit");
    let (nojit, out_n) = time_with(&[("HELIX_NOJIT", "1")], "let_kernel_nojit");
    assert_eq!(out_j, out_n, "engines disagree on the let-body fold");
    let ratio = nojit / jit.max(1e-9);
    assert!(
        ratio > 4.0,
        "the let-in-reduce kernel is not engaging: NOJIT/JIT = {ratio:.1}x \
         (jit={jit:.3}s nojit={nojit:.3}s) — ~1.0x means the guard rejects again"
    );
}

/// The `let`-body fold's semantics, pinned on all three engines: sequential bindings
/// (later sees earlier), an Int-typed local promoting at the interpreter's exact
/// point, NESTED shadowing restoring on scope exit, a local shadowing an outer
/// global (outer intact after), a local coefficient in the captured-array kernel, a
/// let-local in an INDEX (declines to the general path, answers identically), and an
/// accumulator-reading local (likewise). Byte-identical against the pre-change
/// released binary at landing.
#[test]
fn let_in_reduce_semantics_are_pinned() {
    let src = r#"
print(range(0, 5).reduce(0.0, (a, i) => let d = i * 1.0 in a + d * d))
print(range(0, 5).reduce(0.0, (a, i) => let x = i * 1.0, y = x + 1.0 in a + x * y))
print(range(0, 4).reduce(0.0, (a, i) => let k = 2 in a + (i * k) * 1.0))
print(range(0, 4).reduce(0.0, (a, i) => let u = 1.0 in a + (let u = 2.0 in u) + u))
c = 10.0
print(range(0, 4).reduce(0.0, (a, i) => let c = 1.0 in a + c), c)
xs = [1.0, 2.0, 3.0, 4.0]
print(range(0, 4).reduce(0.0, (a, j) => let w = 2.0 in a + xs[j] * w))
print(range(0, 4).reduce(0.0, (a, j) => let k = 1 in a + xs[j * k]))
r = try range(0, 3).reduce(0.0, (a, i) => let d = a in d + 1.0)
print(r.ok)
"#;
    let want = "30.0\n40.0\n12.0\n12.0\n4.0 10.0\n20.0\n10.0\ntrue\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("let_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: let-in-reduce semantics drifted");
    }
}

/// `.mean()` and `.product()` on tracked arrays fold on the tape (the v0.2.5 field
/// re-verification's finding: `.sum()` was closed while these still forked by
/// capability). `mean` is fold-add then a differentiable divide by the count —
/// gradient 1/n exactly; `product`'s fold accumulates repeated-element gradients
/// (`[a, b, a]` gives d/da = 2ab). `.max()`/`.min()` stay open by design until the
/// tie-subgradient decision (docs/dx-plan.md). The same program pins the finding the
/// probe surfaced: `variable(tensor)` ALREADY differentiates through `matmul` — the
/// gradient below is the hand-derived row-sums + col-sums — so tensor-aware autodiff
/// exists and must not regress while it is still undocumented in the field.
#[test]
fn tracked_aggregates_and_tensor_variables_carry_gradients() {
    let src = r#"
a = variable(2.0)
b = variable(6.0)
m = [a, b].mean()
print(value_of(m), gradient(m, a), gradient(m, b))
a2 = variable(2.0)
b2 = variable(5.0)
p = [a2, b2, a2].product()
print(value_of(p), gradient(p, a2), gradient(p, b2))
print([1.0, 2.0, 3.0].mean(), [2, 3, 4].product())
w = variable(tensor([[1.0, 2.0], [3.0, 4.0]]))
g = gradient(w.matmul(w).sum(), w)
print(g)
"#;
    let want = "4.0 0.5 0.5\n20.0 20.0 4.0\n2.0 24\n[[7, 11],\n [9, 13]]\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("agg_tape_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}: tracked aggregates or tensor tape drifted");
    }
}

/// `body_raises` covers `Let` (the v0.2.6 stabilization sweep's two criticals, one
/// root cause). The predicate decides whether a kernel is built WITH its poison cell;
/// the `let` widening admitted `Let` bodies into the f64 analyses and codegen but this
/// fn's `_ => false` answered for them, so the kernel was built poison-free — a user-fn
/// call in a `let` init then hit the mixed-call codegen's unreachable! (SIGABRT, rc
/// 134, uncatchable — the ADR 0024 violation class), and a division by zero under a
/// `let` silently printed `inf` at rc 0 where both interpreters raise (the
/// silent-wrong-answer class, WORSE). Both shapes pinned on all three engines, plus
/// the raising-rounder family and the captured-array control that already worked.
#[test]
fn let_bodies_carry_their_poison_cell() {
    // The SIGABRT shape: value identical everywhere, process alive.
    let src = "fn sq(v) = v * v\n\
               print(range(0, 4).reduce(0.0, (a, i) => let d = sq(i * 1.0) in a + d))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("poison_call_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "14.0\n", "{name}");
    }

    // The silent-inf shape: the division must RAISE, identically.
    let div = "print(range(0, 100).reduce(0.0, (a, i) => \
               let inv = 1.0 / ((i - 50) * 1.0) in a + inv))\n";
    for (name, env) in ENGINES {
        let (_, err, code) = run_source(div, env, &format!("poison_div_{name}"));
        assert_eq!(code, Some(1), "{name}: div-by-zero must raise, not print inf");
        assert!(err.contains("division by zero"), "{name}: {err}");
    }

    // Try-visibility and the rounder family, value-pinned across engines.
    let sem = r#"
q = try range(0, 100).reduce(0.0, (a, i) => let d = 1.0 / (i * 1.0) in a + d)
print(q.ok)
r = try range(0, 4).reduce(0.0, (a, i) => let d = floor(exp(700.0 + i * 100.0)) in a + d)
print(r.ok)
"#;
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(sem, env, &format!("poison_sem_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "false\nfalse\n", "{name}");
    }
}

/// A tracked elementwise op refuses exactly where the plain path refuses. `ew`'s
/// defensive fallback used to answer for a user's shape mistake instead: the forward
/// silently returned the LHS unchanged (where the plain twin raises the broadcast
/// error), and the backward pass then panicked on the mismatched accumulation —
/// SIGABRT rc 134, uncatchable, on every engine. Same guard, both symptoms.
#[test]
fn tracked_shape_mismatch_raises_like_the_plain_path() {
    let fwd = "v = variable(tensor([1.0, 2.0]))\n\
               print(value_of(v + tensor([10.0, 20.0, 30.0])))\n";
    let bwd = "v = variable(tensor([1.0, 2.0]))\n\
               print(gradient((v + tensor([1.0, 2.0, 3.0])).sum(), v))\n";
    for (name, env) in ENGINES {
        for (tag, src) in [("fwd", fwd), ("bwd", bwd)] {
            let (out, err, code) = run_source(src, env, &format!("adshape_{tag}_{name}"));
            assert_eq!(code, Some(1), "{name}/{tag}: must raise, not fabricate: {out}");
            assert!(
                err.contains("cannot broadcast tensors of shape [2] and [3]"),
                "{name}/{tag}: {err}"
            );
        }
    }
    // Legitimate broadcasting still differentiates: the bias-add shape.
    let ok = "w = variable(tensor([[1.0, 2.0], [3.0, 4.0]]))\n\
              b = variable(tensor([5.0, 6.0]))\n\
              print(gradient((w + b).sum(), b))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(ok, env, &format!("adshape_ok_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "[2, 2]\n", "{name}");
    }
}

/// Three silent-wrong-gradient shapes from the stabilization sweep, one test.
/// (1) A tracked EXPONENT refuses rather than freezing at its value — reading it as
/// a constant dropped it from the graph, so `gradient(2.0 ** x, x)` answered 0.0
/// where the truth is 2^x·ln 2. (2) A leaf that does not feed the loss answers 0,
/// not whatever an earlier backward pass left in its grad cell — the training-loop
/// footgun. (3) d/dx x^0 is 0 everywhere, including x = 0 (was 0·inf = NaN).
#[test]
fn tracked_gradients_answer_for_the_current_tape_only() {
    let pow = "x = variable(3.0)\n\
               print(gradient(x ** 2.0, x))\n\
               y = variable(1.0)\n\
               print(gradient(2.0 ** y, y))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(pow, env, &format!("adpow_{name}"));
        assert_eq!(code, Some(1), "{name}: tracked exponent must refuse: {out}");
        assert_eq!(out, "6.0\n", "{name}: constant exponent must still work");
        assert!(err.contains("constant scalar power"), "{name}: {err}");
    }
    let stale = "x = variable(2.0)\n\
                 y = variable(3.0)\n\
                 print(gradient(x * x * y, x))\n\
                 print(gradient(x * x, y))\n\
                 print(gradient(x * x, [x, y]))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(stale, env, &format!("adstale_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "12.0\n0.0\n[4.0, 0.0]\n", "{name}");
    }
    let pow0 = "x = variable(0.0)\nprint(gradient(x ** 0, x))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(pow0, env, &format!("adpow0_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "0.0\n", "{name}");
    }
}

/// Join output order is pinned (`MaintainOrderJoin::LeftRight`), because `.column()`
/// re-executes the lazy plan: with the backend's per-execution ordering, two column
/// reads of ONE grouped-after-join frame paired keys from one run with values from
/// another — silently, at exit 0, in most multi-group runs. The sort-tearing class
/// ADR 0020 exists to forbid, realized through the join path. Repeated runs must be
/// byte-stable AND correctly paired (true sums: a = 1+3 = 4, b = 2).
#[test]
fn grouped_aggregation_after_join_is_deterministic() {
    let src = "left = dataframe({id: [\"a\", \"b\", \"a\"], v: [1.0, 2.0, 3.0]})\n\
               right = dataframe({id: [\"a\", \"b\"], w: [10.0, 20.0]})\n\
               g = left.join(right, @id).group(@id).sum(@v)\n\
               print(g.column(\"id\"))\n\
               print(g.column(\"v\"))\n";
    let want = "[\"a\", \"b\"]\n[4.0, 2.0]\n";
    for i in 0..12 {
        let (out, err, code) = run_source(src, &[], &format!("jointear_{i}"));
        assert_eq!(code, Some(0), "run {i}: {err}");
        assert_eq!(out, want, "run {i}: torn or reordered");
    }
    for (name, env) in ENGINES {
        let (out, _, code) = run_source(src, env, &format!("jointear_eng_{name}"));
        assert_eq!(code, Some(0), "{name}");
        assert_eq!(out, want, "{name}");
    }
}

/// The `helix test` walk is safe on an ordinary filesystem: a symlinked directory
/// cycle terminates (one self-loop used to count the same test 41 times at rc 0, two
/// loops hung forever), overlapping roots count each test and doc example once (the
/// doc side had no cross-root dedup, so `helix test dir dir/file.helix` inflated the
/// pass total), and a doc example whose last line is already `print(...)` passes
/// (the harness double-wrapped it, emitting the value plus print's Unit return).
#[test]
fn helix_test_walk_handles_cycles_overlaps_and_bare_prints() {
    let dir = std::env::temp_dir().join("helix_walk_pins");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("z_test.helix"), "assert_eq(2 + 2, 4)\n").unwrap();
    std::fs::write(
        dir.join("mod_doc.helix"),
        "## Doubles.\n## >>> 2 + 2\n## 4\nfn a(x) = x\n\n\
         ## Already printing.\n## >>> print(1 + 2)\n## 3\nfn b(x) = x\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(".", dir.join("loop_a")).unwrap();
        std::os::unix::fs::symlink(".", dir.join("loop_b")).unwrap();
    }
    let root = dir.to_str().unwrap();
    let (out, err, code) = run(&["test", root], &[], "");
    assert_eq!(code, Some(0), "stderr: {err}\nout: {out}");
    assert!(out.contains("running 1 test file"), "out:\n{out}");
    assert!(out.contains("3 passed"), "out:\n{out}");

    // Overlapping spellings of the same tree all count each test exactly once.
    let file = dir.join("mod_doc.helix");
    for roots in [vec![root, root], vec![root, file.to_str().unwrap()]] {
        let mut args = vec!["test"];
        args.extend(roots.iter().copied());
        let (out, err, code) = run(&args, &[], "");
        assert_eq!(code, Some(0), "{roots:?}: stderr: {err}\nout: {out}");
        assert!(out.contains("3 passed"), "{roots:?}: out:\n{out}");
    }
}

/// The scalars→tensor bridge: `tensor(…)` over tracked scalars builds a TRACKED
/// tensor, so a trainable layer's weights can be ordinary variables and its forward
/// pass an ordinary BLAS `matmul`. Before this, the nn field build had to choose
/// between a differentiable network and a fast one.
///
/// The gradients here are hand-derived, not recorded: for `W·x` summed, ∂/∂wᵢⱼ = xⱼ,
/// so `W = [[w11, w12], [w21, w22]]` against `x = [1, 2]` gives `[1, 2, 1, 2]`.
#[test]
fn tensor_builds_from_tracked_scalars() {
    let layer = "w11 = variable(0.5)\n\
                 w12 = variable(-0.5)\n\
                 w21 = variable(0.25)\n\
                 w22 = variable(0.75)\n\
                 W = tensor([[w11, w12], [w21, w22]])\n\
                 x = tensor([1.0, 2.0])\n\
                 loss = W.matmul(x).sum()\n\
                 print(value_of(loss))\n\
                 print(gradient(loss, [w11, w12, w21, w22]))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(layer, env, &format!("bridge_layer_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "1.25\n[1.0, 2.0, 1.0, 2.0]\n", "{name}");
    }

    // One variable used TWICE in the same build: both slices of the incoming
    // gradient must reach it. `t = [w, w]`, loss = sum(t·t) = 2w², d/dw = 4w = 12.
    let repeated = "w = variable(3.0)\n\
                    t = tensor([w, w])\n\
                    print(value_of((t * t).sum()))\n\
                    print(gradient((t * t).sum(), w))\n";
    // Plain elements become constants, so gradient reaches only the tracked ones.
    let mixed = "w = variable(5.0)\n\
                 t = tensor([w, 2.0])\n\
                 print(value_of(t.sum()))\n\
                 print(gradient(t.sum(), w))\n";
    // Ints convert exactly as the plain build converts them; a bare tracked scalar
    // passes straight through; nesting goes as deep as it likes.
    let edges = "w = variable(1.5)\n\
                 print(value_of(tensor([w, 2]).sum()))\n\
                 print(value_of(tensor(w)))\n\
                 t3 = tensor([[[w, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]])\n\
                 print(gradient(t3.sum(), w))\n";
    for (name, env) in ENGINES {
        for (tag, src, want) in [
            ("repeated", repeated, "18.0\n12.0\n"),
            ("mixed", mixed, "7.0\n1.0\n"),
            ("edges", edges, "3.5\n1.5\n1.0\n"),
        ] {
            let (out, err, code) = run_source(src, env, &format!("bridge_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(out, want, "{name}/{tag}");
        }
    }

    // The honest oracle: analytic gradient against a central difference computed by
    // the program itself, through a build whose variables repeat across positions.
    let cd = "fn f(a, b) = ((tensor([[a, b], [b, a]])).matmul(tensor([1.0, 2.0]))).sum()\n\
              w1 = variable(1.5)\n\
              w2 = variable(-0.5)\n\
              L = ((tensor([[w1, w2], [w2, w1]])).matmul(tensor([1.0, 2.0]))).sum()\n\
              h = 0.000001\n\
              print(gradient(L, w1))\n\
              print(round((f(1.5 + h, -0.5) - f(1.5 - h, -0.5)) / (2.0 * h), 4))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(cd, env, &format!("bridge_cd_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "3.0\n3.0\n", "{name}");
    }
}

/// The bridge's other direction: reading a tracked tensor apart keeps the pieces on
/// the tape. `t[i]` scatters its gradient back into the element it came from, and a
/// slice gathers rows whose adjoint accumulates — so a weight can be built from
/// scalars, multiplied as a matrix, and inspected element-wise without the tape
/// silently ending. Metadata (`shape`/`count`/`ndim`) reads the value, because how
/// big something is was never a question about derivatives.
#[test]
fn tracked_tensors_index_slice_and_measure() {
    let idx = "v = variable(tensor([1.0, 2.0, 3.0]))\n\
               print(value_of(v[0]))\n\
               print(value_of(v[-1]))\n\
               print(gradient(v[1], v))\n";
    let two_d = "W = variable(tensor([[1.0, 2.0], [3.0, 4.0]]))\n\
                 print(value_of(W[1]))\n\
                 print(value_of(W[0][1]))\n\
                 print(gradient(W[0][1], W))\n";
    let slice = "v = variable(tensor([1.0, 2.0, 3.0]))\n\
                 print(value_of(v[0:2]))\n\
                 print(gradient(v[0:2].sum(), v))\n\
                 print(value_of(v[::-1]))\n\
                 print(value_of(v[2:2]))\n";
    // Round trip: build from scalars, then read one element back out.
    let round = "a = variable(2.0)\n\
                 b = variable(3.0)\n\
                 t = tensor([a, b])\n\
                 print(gradient(t[0] * t[1], a))\n\
                 print(gradient(t[1:2].sum(), b))\n\
                 print(gradient(t[1:2].sum(), a))\n";
    let meta = "w = variable(1.0)\n\
                t = tensor([[w, 2.0], [3.0, 4.0]])\n\
                print(t.shape())\n\
                print(t.count())\n\
                print(t.ndim())\n\
                print(w.shape())\n";
    for (name, env) in ENGINES {
        for (tag, src, want) in [
            ("idx", idx, "1.0\n3.0\n[0, 1, 0]\n"),
            ("two_d", two_d, "[3, 4]\n2.0\n[[0, 1],\n [0, 0]]\n"),
            ("slice", slice, "[1, 2]\n[1, 1, 0]\n[3, 2, 1]\n[]\n"),
            ("round", round, "3.0\n1.0\n0.0\n"),
            ("meta", meta, "[2, 2]\n4\n2\n[]\n"),
        ] {
            let (out, err, code) = run_source(src, env, &format!("bridge_read_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(out, want, "{name}/{tag}");
        }
    }
}

/// A mistake reads the same whether or not a variable happens to be in the array.
/// The tracked build refuses exactly what the plain build refuses, in the plain
/// build's words — the programs below compare the two error texts in-language, so a
/// future edit to either wording fails here rather than in a field report. The one
/// refusal with its own text is a tracked TENSOR as an element, where naming the
/// internal type would be worse than useless.
#[test]
fn tracked_tensor_build_refuses_exactly_what_the_plain_build_refuses() {
    let same = |bad_tracked: &str, bad_plain: &str| {
        format!(
            "w = variable(1.0)\n\
             r = try {bad_tracked}\n\
             p = try {bad_plain}\n\
             print(r.ok)\n\
             print(p.ok)\n\
             print(r.error == p.error)\n\
             print(r.error)\n"
        )
    };
    let cases = [
        ("string", same("tensor([w, \"a\"])", "tensor([1.0, \"a\"])"),
         "cannot build a tensor from a value of type String"),
        ("bool", same("tensor([w, true])", "tensor([1.0, true])"),
         "cannot build a tensor from a value of type Bool"),
        ("tuple", same("tensor([w, (1, 2)])", "tensor([1.0, (1, 2)])"),
         "cannot build a tensor from a value of type Tuple"),
        ("record", same("tensor([w, {a: 1}])", "tensor([1.0, {a: 1}])"),
         "cannot build a tensor from a value of type Record"),
        ("missing", same("tensor([w, missing])", "tensor([1.0, missing])"),
         "cannot build a tensor from a value of type Missing"),
        ("ragged", same("tensor([[w], [w, w]])", "tensor([[1.0], [2.0, 3.0]])"),
         "tensor rows must all have the same shape (ragged array)"),
        // The three shapes of "these rows do not agree" — an EMPTY sibling, a
        // scalar beside an array, and an array beside a scalar — all report as the
        // one ragged error, tracked or plain.
        ("empty_sibling", same("tensor([[w], []])", "tensor([[1.0], []])"),
         "tensor rows must all have the same shape (ragged array)"),
        ("scalar_sibling", same("tensor([w, [2.0]])", "tensor([1.0, [2.0]])"),
         "tensor rows must all have the same shape (ragged array)"),
        ("array_sibling", same("tensor([[w], 2.0])", "tensor([[1.0], 2.0])"),
         "tensor rows must all have the same shape (ragged array)"),
        ("deep_ragged", same("tensor([[[w]], [[1.0, 2.0]]])", "tensor([[[3.0]], [[1.0, 2.0]]])"),
         "tensor rows must all have the same shape (ragged array)"),
        ("bounds", same("variable(tensor([1.0, 2.0]))[9]", "tensor([1.0, 2.0])[9]"),
         "index 9 is out of bounds for a tensor axis of length 2"),
        ("index0d", same("variable(1.0)[0]", "tensor(1.0)[0]"),
         "cannot index a 0-D (scalar) tensor"),
        ("slice0d", same("variable(1.0)[0:1]", "tensor(1.0)[0:1]"),
         "cannot slice a 0-D (scalar) tensor"),
    ];
    for (name, env) in ENGINES {
        for (tag, src, text) in &cases {
            let (out, err, code) = run_source(src, env, &format!("bridge_refuse_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(out, format!("false\nfalse\ntrue\n{text}\n"), "{name}/{tag}");
        }
    }

    // A tensor is not a tensor ELEMENT — the rule the plain build already applies —
    // and a tracked tensor is reported as the `Tensor` it is to anyone who can see
    // it. One sentence whichever side of the comma it sits on: reporting the
    // tracked one differently made the same mistake read two ways depending on
    // literal order, and leaked the internal name `Node` while doing it.
    let orders = [
        ("tracked_first", "tensor([r1, tensor([3.0, 4.0])])"),
        ("plain_first", "tensor([tensor([3.0, 4.0]), r1])"),
        ("both_plain", "tensor([tensor([1.0, 2.0]), tensor([3.0, 4.0])])"),
    ];
    for (name, env) in ENGINES {
        for (tag, expr) in orders {
            let block = format!(
                "r1 = variable(tensor([1.0, 2.0]))\n\
                 r = try {expr}\n\
                 print(r.ok)\n\
                 print(r.error)\n\
                 print(r.error.contains(\"Node\"))\n"
            );
            let (out, err, code) =
                run_source(&block, env, &format!("bridge_block_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(
                out,
                "false\ncannot build a tensor from a value of type Tensor\nfalse\n",
                "{name}/{tag}"
            );
        }
    }
}

/// A ragged row outranks a bad element type — in BOTH builds. The plain shape walk
/// interleaves the two checks and stops at the first offending element; the tracked
/// build must do the same, or wrapping one element in `variable(` would change which
/// of two mistakes a program reports.
#[test]
fn the_first_mistake_wins_in_both_tensor_builds() {
    for (name, env) in ENGINES {
        for (tag, bad) in [("string", "\"x\""), ("missing", "missing")] {
            let src = format!(
                "w = variable(1.0)\n\
                 t = try tensor([[w], [2.0, 3.0], {bad}])\n\
                 p = try tensor([[1.0], [2.0, 3.0], {bad}])\n\
                 print(t.error == p.error)\n\
                 print(p.error)\n"
            );
            let (out, err, code) =
                run_source(&src, env, &format!("bridge_order_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(
                out,
                "true\ntensor rows must all have the same shape (ragged array)\n",
                "{name}/{tag}"
            );
        }
    }
}

/// Nothing a program without variables does may change. The plain build is a shape
/// walk and a memcpy, and the bridge is gated behind a predicate that a packed
/// buffer answers without one — so these must read exactly as they did before.
#[test]
fn plain_tensor_construction_and_reads_are_unchanged() {
    let src = "print(tensor([1.0, 2.0]))\n\
               print(tensor([[1, 2], [3, 4]]))\n\
               print(tensor(3.5))\n\
               print(tensor([]))\n\
               print(tensor([1.0, 2.0]).shape())\n\
               print(to_array(tensor([[1.0, 2.0], [3.0, 4.0]])))\n\
               print(tensor([[1.0, 2.0], [3.0, 4.0]])[0])\n\
               print(tensor([1.0, 2.0, 3.0])[-1])\n\
               print(tensor([1.0, 2.0, 3.0])[0:2])\n\
               print(tensor([1.0, 2.0, 3.0])[::-1])\n";
    let want = "[1, 2]\n\
                [[1, 2],\n [3, 4]]\n\
                3.5\n\
                []\n\
                [2]\n\
                [1.0, 2.0, 3.0, 4.0]\n\
                [1, 2]\n\
                3.0\n\
                [1, 2]\n\
                [3, 2, 1]\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("plain_tensor_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }
}

/// Metadata answers the same on tracked and plain data — `count` is TOTAL elements
/// on a tensor (not the leading axis), and a tracked tensor must not quietly pick
/// the other meaning. Also pins the one place the two paths legitimately differ:
/// indexing a 1-D PLAIN tensor yields a `Float`, whose own indexing error names
/// `Float`, while the tracked twin yields a 0-D node and says so. Both are true of
/// the value in hand; neither says `Node`.
#[test]
fn tracked_metadata_matches_plain_and_the_one_honest_difference_is_pinned() {
    let parity = "w = variable(1.0)\n\
                  t2 = tensor([[w, 2.0], [3.0, 4.0]])\n\
                  p2 = tensor([[1.0, 2.0], [3.0, 4.0]])\n\
                  t3 = tensor([[[w, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]])\n\
                  p3 = tensor([[[1.0, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]])\n\
                  print(t2.count() == p2.count())\n\
                  print(t2.shape() == p2.shape())\n\
                  print(t2.ndim() == p2.ndim())\n\
                  print(t3.count() == p3.count())\n\
                  print(t3.shape() == p3.shape())\n\
                  print(\"{t2.count()} {t3.count()}\")\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(parity, env, &format!("bridge_meta_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\n4 8\n", "{name}");
    }
    let chained = "w = variable(1.0)\n\
                   r = try tensor([w, 2.0])[0][1]\n\
                   p = try tensor([1.0, 2.0])[0][1]\n\
                   print(r.error)\n\
                   print(p.error)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(chained, env, &format!("bridge_chain_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "cannot index a 0-D (scalar) tensor\na value of type Float cannot be indexed\n",
            "{name}"
        );
    }
}

/// A wholly plain subtree inside a tracked build is ONE constant block, not one node
/// per number. Without that, `tensor([big_plain_row, [w, …]])` would box a whole row
/// into leaves — the allocation pathology `tests/corpus/j8_tensor_construction.helix`
/// exists to document as fixed, reintroduced through the tracked door.
#[test]
fn a_plain_row_inside_a_tracked_build_stays_one_block() {
    let src = "w = variable(1.0)\n\
               n = 100000\n\
               a = range(0, n).map(i => i * 1.0)\n\
               b = range(0, n).map(i => i * 2.0)\n\
               M = tensor([a, b])\n\
               print(M.shape())\n\
               tracked = tensor([w, 2.0])\n\
               print(value_of(tracked.sum()))\n\
               print(gradient(tracked.sum(), w))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("bridge_block_perf_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "[2, 100000]\n3.0\n1.0\n", "{name}");
    }
}

/// `try` takes an expression and evaluates it; every other language's equivalent
/// takes a callback, so `try(() => f())` is what a newcomer writes — and it used to
/// report SUCCESS, because building a closure cannot fail. The record came back
/// `{ok: true, value: <function/0>}`, so error handling written that way never fired.
/// A 13-library review hit it and first read it as a bug in `try` itself.
#[test]
fn try_refuses_a_function_literal() {
    for (name, env) in ENGINES {
        for (tag, src) in [
            ("bare", "print(try(() => raise(\"x\")))\n"),
            ("call", "fn f() = 1\nprint(try(() => f()))\n"),
        ] {
            let (_, err, code) = run_source(src, env, &format!("try_lambda_{tag}_{name}"));
            assert_eq!(code, Some(1), "{name}/{tag}: must refuse");
            assert!(
                err.contains("`try` takes an expression to evaluate, not a function"),
                "{name}/{tag}: {err}"
            );
        }
        // Everything `try` is FOR still works, including expressions that contain
        // lambdas of their own — the guard is about `try`'s own operand, nothing else.
        let ok = "r = try (1 / 0)\n\
                  print(r.ok)\n\
                  print(r.error)\n\
                  m = try [1, 2].map(x => x * 2)\n\
                  print(m.value)\n\
                  d = try [1, 2].reduce(0, (a, x) => a + x)\n\
                  print(d.value)\n\
                  f = (x) => x + 1\n\
                  print(f(1))\n";
        let (out, err, code) = run_source(ok, env, &format!("try_ok_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "false\ndivision by zero\n[2, 4]\n3\n2\n", "{name}");
    }
}

/// A body that is exactly a call to itself with its parameters unchanged can never
/// return, whatever it is called with. The shape turns up when a name shadows a
/// builtin — `fn relu(x) = relu(x)`, written to wrap the builtin — where the call
/// resolves to the definition being written; the review nearly shipped that one.
/// Real recursion, including recursion under a shadowed builtin name, is untouched.
#[test]
fn a_function_whose_body_is_its_own_call_is_refused() {
    let bad = [
        ("builtin", "fn relu(x) = relu(x)\n", "not the built-in of the same name"),
        ("plain", "fn myf(x) = myf(x)\n", "needs a base case"),
        ("two_params", "fn g(a, b) = g(a, b)\n", "needs a base case"),
    ];
    for (name, env) in ENGINES {
        for (tag, src, hint) in &bad {
            let (_, err, code) = run_source(src, env, &format!("selfcall_{tag}_{name}"));
            assert_eq!(code, Some(1), "{name}/{tag}: must refuse");
            assert!(err.contains("calls itself with the same arguments"), "{name}/{tag}: {err}");
            assert!(err.contains(hint), "{name}/{tag}: {err}");
        }
        // Recursion that makes progress, a shadowed builtin that recurses properly,
        // a shadowed builtin that does not recurse, mutual tail recursion, and a
        // self-call with the arguments SWAPPED — none of these are the refused shape.
        let good = "fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\n\
                    fn abs(x) = if x < 0.0 then abs(0.0 - x) else x\n\
                    fn relu(x) = if x > 0.0 then x else 0.0\n\
                    fn ev(n) = if n == 0 then true else od(n - 1)\n\
                    fn od(n) = if n == 0 then false else ev(n - 1)\n\
                    fn swap(a, b) = if a <= 0 then b else swap(b - 1, a - 1)\n\
                    print(fact(5))\n\
                    print(abs(0.0 - 3.0))\n\
                    print(relu(0.0 - 2.0))\n\
                    print(ev(10))\n\
                    print(swap(2, 3))\n";
        let (out, err, code) = run_source(good, env, &format!("selfcall_ok_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "120\n3.0\n0.0\ntrue\n1\n", "{name}");
    }
}

/// A `fn` may be called above its definition; a top-level value may not be used above
/// its binding. Nothing said so, so a module with its constant table at the bottom
/// failed at every function above it — one root cause wearing a "not defined" face at
/// each use. The name is bound further down, and the message now says exactly that,
/// while a name that is nowhere in the file still reads as plainly undefined.
#[test]
fn a_binding_used_before_its_definition_says_so() {
    for (name, env) in ENGINES {
        let (_, err, code) = run_source("print(K)\nK = 5\n", env, &format!("later_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains("`K` is not defined yet"), "{name}: {err}");
        assert!(err.contains("bound further down this file"), "{name}: {err}");
        assert!(err.contains("move the binding up"), "{name}: {err}");

        let (_, err, code) = run_source("print(NOPE)\n", env, &format!("never_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains("`NOPE` is not defined"), "{name}: {err}");
        assert!(!err.contains("not defined yet"), "{name}: {err}");

        // A `fn` used above its definition is still fine — the asymmetry the message
        // describes is real, and this is the half that works.
        let (out, err, code) =
            run_source("print(f(2))\nfn f(x) = x * 2\n", env, &format!("fnfirst_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "4\n", "{name}");
    }
}

/// Splitting on the FIRST separator is the commonest parsing step there is, and it
/// had no spelling: the idiom was `let eq = part.split("="), k = eq[0], v = if
/// eq.count() <= 1 then "" else part.drop(k.count() + 1)` — split everything, discard
/// the rest, recover the tail by arithmetic on the first part's length — repeated in
/// five modules of one corpus, each an off-by-one waiting to happen.
///
/// `index_of` answers in CHARACTERS, the unit every other String method counts in, so
/// the index can be fed straight back to `drop`/`take`/`s[a:b]`. Answering in bytes
/// would work on ASCII and silently mislocate on anything else; the pin below proves
/// it agrees with `chars().position(…)` on a multi-byte string.
#[test]
fn string_search_answers_in_characters() {
    let src = "print(\"hello\".index_of(\"l\"))\n\
               print(\"hello\".index_of(\"z\"))\n\
               print(\"héllo\".index_of(\"l\"))\n\
               print(\"héllo\".chars().position(c => c == \"l\"))\n\
               k = \"key=val\".index_of(\"=\")\n\
               print(\"key=val\".drop(k + 1))\n\
               print(\"abc\".index_of(\"\"))\n\
               print(\"abc\".contains(\"\"))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("index_of_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "2\nmissing\n2\n2\nval\n0\ntrue\n", "{name}");
    }

    let once = "print(\"a=b=c\".split_once(\"=\"))\n\
                print(\"abc\".split_once(\"=\"))\n\
                print(\"k=\".split_once(\"=\"))\n\
                k, v = \"key=val=x\".split_once(\"=\")\n\
                print(k)\n\
                print(v)\n\
                r = try (\"a\".split_once(\"\"))\n\
                print(r.error)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(once, env, &format!("split_once_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "(\"a\", \"b=c\")\nmissing\n(\"k\", \"\")\nkey\nval=x\n\
             `split_once` separator cannot be empty\n",
            "{name}"
        );
    }

    // The idiom it replaces, and the replacement, on the same input.
    let both = "part = \"key=val=x\"\n\
                eq = part.split(\"=\")\n\
                a = eq[0]\n\
                b = if eq.count() <= 1 then \"\" else part.drop(a.count() + 1)\n\
                c, d = part.split_once(\"=\")\n\
                print(a == c)\n\
                print(b == d)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(both, env, &format!("split_once_idiom_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "true\ntrue\n", "{name}");
    }
}

/// `Dna` has had `windows` since the bio work; an array had to hand-roll
/// `range(0, len - n + 1).map(i => xs.drop(i).take(n))` — two intermediate arrays per
/// window — and signal processing and k-mer scanning both did. `windows` slides and
/// overlaps; `chunks` partitions, and its last group is SHORT when the length does
/// not divide evenly, because dropping it would silently lose data.
#[test]
fn arrays_can_window_and_chunk() {
    let src = "print([1, 2, 3, 4].windows(2))\n\
               print([1, 2, 3, 4].windows(3))\n\
               print([1, 2].windows(5))\n\
               print([].windows(2))\n\
               print([1, 2, 3, 4, 5].chunks(2))\n\
               print([1, 2, 3, 4].chunks(2))\n\
               print([].chunks(2))\n\
               r = try ([1, 2].windows(0))\n\
               print(r.error)\n";
    let want = "[[1, 2], [2, 3], [3, 4]]\n\
                [[1, 2, 3], [2, 3, 4]]\n\
                []\n\
                []\n\
                [[1, 2], [3, 4], [5]]\n\
                [[1, 2], [3, 4]]\n\
                []\n\
                `windows` needs a positive size, got 0\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("windows_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }

    // A moving average, and agreement with the hand-rolled spelling it replaces.
    let use_it = "xs = [1.0, 2.0, 3.0, 4.0, 5.0]\n\
                  print(xs.windows(3).map(w => w.mean()))\n\
                  ys = [1, 2, 3, 4]\n\
                  print(range(0, ys.count() - 1).map(i => ys.drop(i).take(2)) == ys.windows(2))\n\
                  print(ys.chunks(2).flatten() == ys)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(use_it, env, &format!("windows_use_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "[2.0, 3.0, 4.0]\ntrue\ntrue\n", "{name}");
    }
}

/// A pair is a pair however it is written. `(k, v)` stays canonical, but a table
/// transcribed from JSON or a reference document arrives as two-element ARRAYS, and
/// refusing those sent people to `reduce(dict(), (d, kv) => d.insert(kv[0], kv[1]))`
/// — a fold standing in for a literal, counted seventeen times in one corpus. The
/// arity is still checked: a three-element row is a mistake, not a pair.
#[test]
fn to_dict_takes_a_pair_however_it_is_written() {
    let src = "print([(\"a\", 1), (\"b\", 2)].to_dict())\n\
               print([[\"a\", 1], [\"b\", 2]].to_dict())\n\
               print([(\"a\", 1), [\"b\", 2]].to_dict())\n\
               print([].to_dict())\n\
               print([\"a\", \"b\", \"a\"].frequencies().to_dict())\n\
               REASONS = [[100, \"Continue\"], [404, \"Not Found\"]].to_dict()\n\
               print(REASONS.get(404))\n";
    let want = "{\"a\" => 1, \"b\" => 2}\n\
                {\"a\" => 1, \"b\" => 2}\n\
                {\"a\" => 1, \"b\" => 2}\n\
                {}\n\
                {\"a\" => 2, \"b\" => 1}\n\
                Not Found\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("to_dict_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }

    // Wrong arity is still wrong, and the message names the shape that was found.
    let bad = "print((try ([[1, 2, 3]].to_dict())).error)\n\
               print((try ([[1]].to_dict())).error)\n\
               print((try ([5].to_dict())).error)\n";
    let bad_want = "`to_dict` needs (key, value) pairs, but element 0 is a 3-element array\n\
                    `to_dict` needs (key, value) pairs, but element 0 is a 1-element array\n\
                    `to_dict` needs (key, value) pairs, but element 0 is an Int\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(bad, env, &format!("to_dict_bad_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, bad_want, "{name}");
    }
}

/// `concat` joins two sequences, and meant that on an Array but did not exist on a
/// String — an asymmetry with no reason a reader could state. Interpolation stays the
/// everyday way to build a string; this is about one verb meaning one thing.
#[test]
fn concat_joins_strings_as_well_as_arrays() {
    let src = "print(\"a\".concat(\"b\"))\n\
               print([1, 2].concat([3]))\n\
               print(\"a\".concat(\"b\").concat(\"c\").upper())\n\
               a = \"x\"\n\
               b = \"y\"\n\
               print(a.concat(b) == \"{a}{b}\")\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("concat_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "ab\n[1, 2, 3]\nABC\ntrue\n", "{name}");
    }
}

/// `match c { 200..300 => "success", … }` — the numeric ladder as the table it always
/// was. A 15,260-line review counted 162 `else if` arms, the deepest of them exactly
/// this shape, and ranked range patterns above `elif` because `elif` flattens a
/// ladder while a range removes it.
///
/// Half-open, `lo <= x < hi`, the convention `range(lo, hi)` and `xs[lo:hi]` already
/// use — so adjacent bands TILE: nothing lands in two of them and nothing falls
/// between. The boundary walk below is the whole argument, and the last case checks
/// 400 values against the `if` ladder it replaces.
#[test]
fn match_range_patterns_tile_half_open() {
    let classes = "fn class_name(c) = match c {\n\
                     100..200 => \"informational\",\n\
                     200..300 => \"success\",\n\
                     300..400 => \"redirect\",\n\
                     400..500 => \"client error\",\n\
                     500..600 => \"server error\",\n\
                     _ => \"unknown\"\n\
                   }\n\
                   print(class_name(204))\n\
                   print(class_name(404))\n\
                   print(class_name(500))\n\
                   print(class_name(700))\n";
    let edges = "fn f(x) = match x { 0..10 => \"a\", 10..20 => \"b\", _ => \"c\" }\n\
                 print(f(0))\n\
                 print(f(9))\n\
                 print(f(10))\n\
                 print(f(19))\n\
                 print(f(20))\n";
    // A range asks about MAGNITUDE, so it takes a number however written; a literal
    // pattern still tests identity within one representation, and `1` is not `1.0`.
    let kinds = "print(match 2.5 { 0..5 => \"in\", _ => \"out\" })\n\
                 print(match 2 { 0.0..5.0 => \"in\", _ => \"out\" })\n\
                 print(match 1.0 { 1 => \"int-lit\", _ => \"no\" })\n\
                 print(match \"x\" { 0..5 => \"in\", _ => \"out\" })\n\
                 print(match missing { 0..5 => \"in\", _ => \"out\" })\n";
    let rest = "print(match 0 - 3 { -5..0 => \"neg\", 0..5 => \"pos\", _ => \"far\" })\n\
                print(match 0 - 7 { -10..-5 => \"band\", _ => \"no\" })\n\
                print(match 7 { n if n > 100 => \"big\", 0..10 => \"small\", _ => \"mid\" })\n\
                print(match 3 { 1 | 2 | 3 => \"low\", _ => \"high\" })\n\
                print(match 42 { 0..10 => \"low\", n => \"got {n}\" })\n";
    // The ladder and the table must agree on every value in range.
    let same = "fn old(c) = if c < 200 then \"i\" else if c < 300 then \"s\" \
                            else if c < 400 then \"r\" else \"o\"\n\
                fn new(c) = match c { 0..200 => \"i\", 200..300 => \"s\", 300..400 => \"r\", _ => \"o\" }\n\
                print(range(100, 500).all(c => old(c) == new(c)))\n";
    for (name, env) in ENGINES {
        for (tag, src, want) in [
            ("classes", classes, "success\nclient error\nserver error\nunknown\n"),
            ("edges", edges, "a\na\nb\nb\nc\n"),
            ("kinds", kinds, "in\nin\nno\nout\nout\n"),
            ("rest", rest, "neg\nband\nsmall\nlow\ngot 42\n"),
            ("same", same, "true\n"),
        ] {
            let (out, err, code) = run_source(src, env, &format!("range_pat_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(out, want, "{name}/{tag}");
        }
    }
}

/// A range that can never match is a typo, not a pattern that happens never to fire,
/// and both bounds are known where it is written — so it is refused there.
#[test]
fn an_impossible_range_pattern_is_refused_where_it_is_written() {
    let cases = [
        ("reversed", "print(match 1 { 5..0 => \"x\", _ => \"y\" })\n", "low bound below its high bound"),
        ("empty", "print(match 1 { 3..3 => \"x\", _ => \"y\" })\n", "low bound below its high bound"),
        ("no_upper", "print(match 1 { 3.. => \"x\", _ => \"y\" })\n", "expected a number after `..`"),
        (
            "inexact",
            "print(match 1 { 0..99999999999999999999.0 => \"x\", _ => \"y\" })\n",
            "too large to be an exact range bound",
        ),
    ];
    for (name, env) in ENGINES {
        for (tag, src, msg) in &cases {
            let (_, err, code) = run_source(src, env, &format!("range_bad_{tag}_{name}"));
            assert_eq!(code, Some(1), "{name}/{tag}: must refuse");
            assert!(err.contains(msg), "{name}/{tag}: {err}");
        }
    }
}

/// The recursion cap only ever binds a NON-tail shape — Helix reuses the frame of a
/// tail call, so a tail-recursive function, including a mutually tail-recursive pair,
/// runs to millions of levels. The message never said so, and read as a flat limit on
/// recursion: a reader went looking for a loop the language does not have, instead of
/// at the one rewrite that removes the limit. A 15,260-line review measured the
/// optimisation, called it good design, and observed it was invisible.
///
/// The interesting half of this test is not the wording — it is that every claim the
/// wording makes is checked here: tail recursion to a million on all three engines,
/// mutual tail recursion likewise, and the exact rewrite the hint suggests turning the
/// failing depth into a working one. A hint that teaches something must be true.
#[test]
fn recursion_depth_error_names_tail_position() {
    let deep = "fn deep(n) = if n == 0 then 0 else 1 + deep(n - 1)\n\
                print(deep(50000))\n";
    for (name, env) in ENGINES {
        let (_, err, code) = run_source(deep, env, &format!("depth_{name}"));
        assert_eq!(code, Some(1), "{name}: 50k non-tail frames must hit the cap");
        assert!(err.contains("maximum recursion depth"), "{name}: {err}");
        assert!(err.contains("not in TAIL position"), "{name}: {err}");
        assert!(err.contains("reuses its frame"), "{name}: {err}");
    }

    // Everything the hint asserts, verified rather than asserted.
    let claims = "fn count_to(n, acc) = if n == 0 then acc else count_to(n - 1, acc + 1)\n\
                  fn ev(n) = if n == 0 then true else od(n - 1)\n\
                  fn od(n) = if n == 0 then false else ev(n - 1)\n\
                  fn deep_tail(n, acc) = if n == 0 then acc else deep_tail(n - 1, acc + 1)\n\
                  print(count_to(1000000, 0))\n\
                  print(ev(1000000))\n\
                  print(deep_tail(50000, 0))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(claims, env, &format!("depth_claims_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "1000000\ntrue\n50000\n", "{name}");
    }
}

/// `x.f(a)` means `f(x, a)` when `f` is no type's method. The review's item 2.4 was
/// that the method-vs-function split has no discoverable rule — `to_array(t)` is a
/// function, `a.matmul(b)` is a method, and you learn which one error at a time. This
/// removes the split rather than documenting it, and in doing so gives a user's own
/// functions the chaining that only built-in types had: `layer.forward(x)` on a plain
/// record, with no second way to define behaviour and no mutable object identity.
#[test]
fn a_function_can_be_called_in_method_position() {
    let src = "fn area(r) = r.w * r.h\n\
               fn scaled(r, k) = {w: r.w * k, h: r.h * k}\n\
               fn double(x) = x * 2\n\
               fn inc(x) = x + 1\n\
               print({w: 3, h: 4}.area())\n\
               print({w: 2, h: 3}.scaled(2).area())\n\
               print(5.double().inc().double())\n\
               print(tensor([1.0, 2.0]).to_array())\n\
               print((0 - 1).abs())\n\
               r = {w: 3, h: 4}\n\
               print(\"area is {r.area()}\")\n\
               print([1, 2, 3].map(x => x.double()))\n";
    let want = "12\n24\n22\n[1.0, 2.0]\n1\narea is 12\n[2, 4, 6]\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("ufcs_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }

    // A function may be called above its definition, and the fallback must know that —
    // the parser pre-scans `fn` names from the token stream for exactly this.
    let below = "print({w: 2, h: 5}.area())\nfn area(r) = r.w * r.h\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(below, env, &format!("ufcs_below_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "10\n", "{name}");
    }
}

/// UFCS is STRICTLY ADDITIVE: it fires only for a name that is no type's method, so
/// nothing that resolves today can change meaning. The cases below are the ones that
/// would notice if that gate ever slipped — a real method must still win over a
/// same-named user function, a misspelled method must still get the method error and
/// its did-you-mean rather than becoming an undefined-function one, and a removed
/// NAMESPACE must keep the migration hint that is a reader's only pointer to the new
/// spelling (the gate caught that one: `stats.t_test(…)` had become `t_test(stats, …)`).
#[test]
fn ufcs_never_changes_a_call_that_already_resolved() {
    // A user function named like an Array method does NOT capture the method call.
    let shadow = "fn count(xs) = 999\n\
                  print([1, 2, 3].count())\n\
                  print(count([1, 2, 3]))\n\
                  print(\"hello\".upper())\n\
                  print([3, 1, 2].sort())\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(shadow, env, &format!("ufcs_shadow_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "3\n999\nHELLO\n[1, 2, 3]\n", "{name}");
    }

    let keeps = [
        ("typo", "print([1, 2].lenght())\n", "type Array has no method `lenght`"),
        ("wrong_recv", "print(\"abc\".windows(2))\n", "type String has no method `windows`"),
        ("unknown", "print([1, 2].nosuchthing())\n", "type Array has no method `nosuchthing`"),
        ("no_fn", "print({a: 1}.nope())\n", "type Record has no method `nope`"),
        ("namespace", "print(stats.t_test([1.0], [2.0]))\n", "no longer available"),
    ];
    for (name, env) in ENGINES {
        for (tag, src, msg) in &keeps {
            let (_, err, code) = run_source(src, env, &format!("ufcs_keeps_{tag}_{name}"));
            assert_eq!(code, Some(1), "{name}/{tag}: must still error");
            assert!(err.contains(msg), "{name}/{tag}: {err}");
        }
    }

    // Arity is the callee's business, and its own error says so.
    let arity = "fn area(r) = r.w * r.h\nprint({w: 1, h: 2}.area(9))\n";
    for (name, env) in ENGINES {
        let (_, err, code) = run_source(arity, env, &format!("ufcs_arity_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains("`area` expects 1 argument, got 2"), "{name}: {err}");
    }
}

/// `#[ … ]#` — a comment that spans lines and NESTS, so a region that already contains
/// one can be commented out whole. Every module header in the reviewed corpus was a
/// 20-line run of `#`, across 117 modules.
///
/// The interesting pin is `statements_survive`: `lex` drops comments, and newlines are
/// significant here, so a block that crossed lines had to leave its break behind or it
/// would have joined the statements either side of it — a comment changing what a
/// program means, which is the one thing a comment may never do.
#[test]
fn block_comments_span_lines_and_nest() {
    let cases = [
        ("header", "#[\n  A module header.\n  Several lines.\n]#\nprint(1)\n", "1\n"),
        ("statements_survive", "a = 1\n#[\n  spanning\n  lines\n]#\nb = 2\nprint(a + b)\n", "3\n"),
        ("inline", "print(1) #[ trailing ]#\nprint(2)\n", "1\n2\n"),
        ("mid_expression", "print(1 + #[ why ]# 2)\n", "3\n"),
        ("nesting", "#[ outer #[ inner ]# still outer ]#\nprint(7)\n", "7\n"),
        ("commenting_out", "#[\nprint(\"never\")\n#[ nested ]#\n]#\nprint(\"only this\")\n", "only this\n"),
        ("empty", "#[]#\nprint(3)\n", "3\n"),
        // Untouched neighbours: a line comment whose text merely starts with `[`, a doc
        // comment (the char after `#` is another `#`), and the sequence inside a string.
        ("line_comment_with_bracket", "# see #[1] in the paper\nprint(9)\n", "9\n"),
        ("in_a_string", "print(\"#[ not a comment ]#\")\n", "#[ not a comment ]#\n"),
    ];
    for (name, env) in ENGINES {
        for (tag, src, want) in &cases {
            let (out, err, code) = run_source(src, env, &format!("block_{tag}_{name}"));
            assert_eq!(code, Some(0), "{name}/{tag}: {err}");
            assert_eq!(out, *want, "{name}/{tag}");
        }
        // Unterminated is a clean error at the line the block OPENED on — `line` has
        // walked to the end of the file by then, and pointing there would be useless.
        let (_, err, code) =
            run_source("x = 1\n#[ never closed\nprint(1)\n", env, &format!("block_unterm_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains("unterminated block comment"), "{name}: {err}");
        assert!(err.contains(":2:1"), "{name}: should point at the opener: {err}");

        // Positions after a multi-line block are still right.
        let (_, err, code) =
            run_source("#[\n\n\n]#\nprint(nope)\n", env, &format!("block_lines_{name}"));
        assert_eq!(code, Some(1), "{name}");
        assert!(err.contains(":5:7"), "{name}: line count drifted: {err}");
    }
}

/// `url_encode` / `url_decode` — RFC 3986 percent-encoding, without which a web
/// library cannot build a correct URL. Before this,
/// `ps.map((k, v) => "{k}={v}").join("&")` produced `q=hello world&n=2`, which is not
/// a query string, and the hand-rolled fix could not be right: `s.chars().map(…)` maps
/// CHARACTERS, and percent-encoding is defined over UTF-8 BYTES — which is the case
/// this test exists for. `café` must be `caf%C3%A9`, two escapes for one character.
#[test]
fn percent_encoding_round_trips_bytes_not_characters() {
    let src = "print(url_encode(\"a b&c=d/e?f\"))\n\
               print(url_encode(\"abcXYZ012-._~\"))\n\
               print(url_encode(\"café\"))\n\
               print(url_decode(url_encode(\"café\")))\n\
               print(url_decode(\"a%20b%26c\"))\n\
               print(url_decode(url_encode(\"hello world & more=x\")))\n\
               print(url_encode(missing))\n";
    // The unreserved set survives untouched; everything else is UPPERCASE hex, which is
    // what the RFC says producers should emit.
    let want = "a%20b%26c%3Dd%2Fe%3Ff\n\
                abcXYZ012-._~\n\
                caf%C3%A9\n\
                café\n\
                a b&c\n\
                hello world & more=x\n\
                missing\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("urlenc_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }

    // A truncated escape is an error, not data: the caller is usually parsing something
    // that arrived over a network, where a silent pass-through hides a real corruption.
    // Space is `%20`, never `+` — `+`-for-space is form encoding, a different thing, and
    // conflating them corrupts a literal plus. Form bodies decode in the other order,
    // which is the last line here.
    let edges = "r = try (url_decode(\"a%2\"))\n\
                 print(r.error)\n\
                 print(url_encode(\"a+b\"))\n\
                 print(url_decode(\"a%2Bb\"))\n\
                 print(url_decode(\"a+b\".replace(\"+\", \" \")))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(edges, env, &format!("urlenc_edge_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "`url_decode` found a `%` that is not followed by two hex digits, at position 1\n\
             a%2Bb\na+b\na b\n",
            "{name}"
        );
    }

    // The whole point, end to end: build a query string and read it back.
    let trip = "ps = [(\"q\", \"hello world\"), (\"tag\", \"a&b\")]\n\
                qs = ps.map((k, v) => \"{url_encode(k)}={url_encode(v)}\").join(\"&\")\n\
                print(qs)\n\
                back = qs.split(\"&\").map(kv => kv.split_once(\"=\"))\n\
                          .map((k, v) => (url_decode(k), url_decode(v))).to_dict()\n\
                print(back.get(\"tag\"))\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(trip, env, &format!("urlenc_trip_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, "q=hello%20world&tag=a%26b\na&b\n", "{name}");
    }
}

/// `{...dict, k: v}` — a Dict spreads into a record. The review's last web-shaped
/// complaint was that it could not, "which forced branch-per-field code in
/// `llm/request.helix`": that is the shape of every request builder, where some fields
/// are known and typed and the rest arrive as a bag of caller options, and the result
/// has to be one object to serialise.
///
/// Only STRING keys can cross, because a record field is a NAME. A key that has no
/// spelling as a field is refused rather than skipped — dropping part of a payload
/// silently is worse than not building it.
#[test]
fn a_dict_spreads_into_a_record() {
    let src = "d = [(\"a\", 1), (\"b\", 2)].to_dict()\n\
               print({...d, c: 3})\n\
               print({...d, a: 99})\n\
               print({...dict(), a: 1})\n\
               print({...{a: 1}, b: 2})\n\
               opts = [(\"temperature\", 0.7), (\"top_p\", 0.9)].to_dict()\n\
               print({...opts, model: \"helix-1\", stream: false}.to_json())\n";
    let want = "{a: 1, b: 2, c: 3}\n\
                {a: 99, b: 2}\n\
                {a: 1}\n\
                {a: 1, b: 2}\n\
                {\"model\":\"helix-1\",\"stream\":false,\"temperature\":0.7,\"top_p\":0.9}\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(src, env, &format!("dictspread_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(out, want, "{name}");
    }

    // A non-name key is only knowable once the dict exists, so this is a RUNTIME
    // error and `try` catches it.
    let bad_key = "d = [(1, \"x\")].to_dict()\n\
                   r = try ({...d, a: 1})\n\
                   print(r.error)\n";
    for (name, env) in ENGINES {
        let (out, err, code) = run_source(bad_key, env, &format!("dictspread_key_{name}"));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(
            out,
            "a record field must be a name, but this dict has the key `1`\n",
            "{name}"
        );
    }

    // An ARRAY base is provable statically, so the checker rejects the program before
    // any engine runs — `try` never gets the chance, which is the correct split.
    let bad_base = "print({...[1, 2], a: 1})\n";
    for (name, env) in ENGINES {
        let (_, err, code) = run_source(bad_base, env, &format!("dictspread_base_{name}"));
        assert_eq!(code, Some(1), "{name}: an array base is a check-time error");
        assert!(err.contains("`...` record update needs a record, got Array"), "{name}: {err}");
        assert!(err.contains("must be a record or a dict"), "{name}: {err}");
    }
}
