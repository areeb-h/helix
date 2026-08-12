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
            assert!(
                err.contains("is immutable and cannot be reassigned"),
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

/// The forward reference is SCOPED to bodies, which is where the tree-walker's
/// call-time resolution makes it observable: a top-level `fn` binds when execution
/// reaches it, so a *top-level* call above the definition still raises, and a name
/// that shadows a builtin is not shadowed retroactively. Pre-registering either
/// would answer programs the walker rejects, or answer them differently.
#[test]
fn forward_reference_does_not_leak_to_top_level_or_shadow_retroactively() {
    // Top level, above the definition: unknown on every engine, as before.
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let (_, err, code) = run_source("print(f(1))\nfn f(x) = x + 1\n", env, "above");
        assert_eq!(code, Some(1), "env {env:?} stderr: {err}");
        assert!(err.contains("`f` is not a known function"), "env {env:?} stderr: {err}");
    }
    // A builtin keeps answering until the shadow's definition is reached.
    for env in [&[][..], &[("HELIX_NOJIT", "1")][..], &[("HELIX_NOVM", "1")][..]] {
        let src = "print(round(1.4))\nfn round(x) = 99\nprint(round(1.4))\n";
        let (out, err, code) = run_source(src, env, "shadow");
        assert_eq!(code, Some(0), "env {env:?} stderr: {err}");
        assert_eq!(out.lines().collect::<Vec<_>>(), vec!["1", "99"], "env {env:?}");
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
        ("src/interp/comprehensions.rs", 4),
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
        ("src/vm.rs", 57),
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

/// One documented example, lifted out of a `##` doc comment.
struct DocExample {
    line: usize,
    /// The `>>>` lines, in order. All but the last are setup.
    code: Vec<String>,
    /// The plain lines beneath, compared exactly against stdout (or stderr, for an error).
    expect: Vec<String>,
    /// Columns of indentation the `>>>` sat at, stripped from the expected lines so the
    /// block can be indented under the prose without that indent becoming part of the
    /// expected output. Deeper indentation inside the output itself is preserved.
    indent: usize,
}

/// Pull every `>>>` example out of the `##` doc comments in one source file.
///
/// The format is deliberately the smallest thing that cannot be ambiguous: inside a `##`
/// block, a line whose first token is `>>>` is code; consecutive `>>>` lines are one
/// program; the plain `##` lines that follow are its expected output, ending at a blank
/// doc line, the next `>>>`, or the end of the block.
fn doc_examples_in(src: &str) -> Vec<DocExample> {
    let mut out: Vec<DocExample> = Vec::new();
    let mut cur: Option<DocExample> = None;
    for (i, raw) in src.lines().enumerate() {
        let t = raw.trim_start();
        // Only `##` lines participate. A plain `#` comment can sit inside an example
        // block without terminating it being ambiguous, because it is not a doc line.
        let Some(body) = t.strip_prefix("##") else {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        };
        let body = body.strip_prefix(' ').unwrap_or(body);
        let trimmed = body.trim_start();
        if let Some(code) = trimmed.strip_prefix(">>>") {
            let indent = body.len() - trimmed.len();
            let code = code.trim().to_string();
            match cur.as_mut() {
                // A `>>>` directly after previous code (no expected output yet) continues
                // the same program; one after expected output starts a new example.
                Some(e) if e.expect.is_empty() => e.code.push(code),
                _ => {
                    if let Some(e) = cur.take() {
                        out.push(e);
                    }
                    cur = Some(DocExample {
                        line: i + 1,
                        code: vec![code],
                        expect: Vec::new(),
                        indent,
                    });
                }
            }
        } else if let Some(e) = cur.as_mut() {
            if trimmed.is_empty() {
                out.push(cur.take().unwrap());
            } else {
                // Strip exactly the `>>>` line's indentation, no more — so output that is
                // itself indented keeps that shape.
                let mut line = body.trim_end();
                for _ in 0..e.indent {
                    line = line.strip_prefix(' ').unwrap_or(line);
                }
                e.expect.push(line.to_string());
            }
        }
    }
    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

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
            let mut prog = src.clone();
            prog.push('\n');
            for (i, line) in ex.code.iter().enumerate() {
                if i + 1 == ex.code.len() && !ex.expect.is_empty() {
                    prog.push_str(&format!("print({line})\n"));
                } else {
                    prog.push_str(line);
                    prog.push('\n');
                }
            }
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
