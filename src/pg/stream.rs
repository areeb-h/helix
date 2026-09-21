//! The connection's bytes: the socket, plain or under TLS, read through ONE buffer that every
//! message is handed out of.
//!
//! THE BUFFER IS WHY A RESULT IS NOT TWO SYSCALLS A ROW. A message is a 5-byte header and then
//! its body, and on a bare socket that was two `read` calls for every `DataRow`: a 1 000-row
//! result, ~2 000 syscalls, most of what that read cost — the field build measured it at 3.7x
//! pgx and took it for text parsing (§1.61). It also made a read's time depend on the server's
//! pacing, a tiny read either finding its bytes or blocking for them: preparing statements,
//! which makes the server answer SOONER, read 6% slower on that row until this.
//!
//! AND A MESSAGE IS BORROWED FROM IT, NOT COPIED OUT. The first version of the buffer sat under
//! `Read`, so each message was still a fresh `Vec` — allocated, zeroed, filled from the buffer,
//! parsed and freed, a thousand times for a thousand rows. [`Stream::next_msg`] lends the body
//! where it already lies; only a message larger than the buffer (a row over 16 KiB) is gathered
//! into a second one, which is kept for the next. Nothing is ever read around the buffer, so
//! nothing can be skipped or seen twice.
//!
//! A REQUEST THAT CAN BE ANSWERED BEFORE IT HAS ALL BEEN SENT — several statements in one
//! flight — IS SENT WHILE ITS ANSWER IS TAKEN IN ([`Stream::send_draining`]). Writing first
//! and reading afterwards is how every single statement goes, and it is safe there because the
//! server has nothing to say until it has read the statement. A flight of many is different:
//! the first statement's rows are on their way while the last is still being written, and if
//! they fill the socket the server stops reading to wait for room — while this client, not
//! reading, waits for the server to read. Neither moves again. So such a flight goes out on a
//! socket that does not block: what the kernel will not take yet waits, what has arrived is
//! set ASIDE (as bytes, or as plaintext under TLS — never parsed here), and when neither
//! direction moves the kernel is asked to say when one can. Every later read serves what was
//! set aside first, so nothing is lost and nothing is reordered.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use rustls::{ClientConnection, StreamOwned};

use super::proto;

/// The socket, before or after TLS wraps it.
enum Raw {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Raw {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Raw::Plain(s) => s.read(buf),
            Raw::Tls(s) => s.read(buf),
        }
    }
}

/// How much of the server's answer is taken from the socket at once. PostgreSQL flushes its
/// own send buffer at 8 KiB, so this holds two of those and nearly every message whole.
const READ_BUFFER: usize = 16 * 1024;

/// A gathered oversize message's buffer is kept for the next one up to this size, and let go
/// past it: one 60 MB `bytea` must not pin 60 MB to the connection for the rest of its life.
const KEEP_GATHERED: usize = 1024 * 1024;

/// What arrived while a request was still going out, waiting to be read.
#[derive(Default)]
struct Aside {
    bytes: Vec<u8>,
    at: usize,
}

impl Aside {
    /// Hand over what was set aside, oldest first. `None` when there is nothing.
    fn serve(&mut self, dst: &mut [u8]) -> Option<usize> {
        let left = self.bytes.get(self.at..).unwrap_or(&[]);
        let n = left.len().min(dst.len());
        if n == 0 {
            return None;
        }
        dst.get_mut(..n)?.copy_from_slice(left.get(..n)?);
        self.at += n;
        if self.at == self.bytes.len() {
            // All read: let it go, however large it grew.
            *self = Aside::default();
        }
        Some(n)
    }
}

/// The connection, before or after TLS wraps it.
///
/// `proto`'s framing writes through `impl Write`, and messages are read with
/// [`Stream::next_msg`], so this is the only place that has to know whether there is a TLS
/// record layer underneath.
pub struct Stream {
    raw: Raw,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    /// A message too large for `buf`, gathered whole.
    gathered: Vec<u8>,
    /// How long a read waits before it gives up — named in the error when it does.
    wait: Option<Duration>,
    /// What a caller can do about a wait that ran out, when there is something.
    way_out: &'static str,
    /// What `send_draining` took in while it was sending.
    aside: Aside,
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.raw {
            Raw::Plain(s) => s.write(buf),
            Raw::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.raw {
            Raw::Plain(s) => s.flush(),
            Raw::Tls(s) => s.flush(),
        }
    }
}

impl Stream {
    fn over(raw: Raw, wait: Option<Duration>) -> Self {
        Stream {
            raw,
            buf: vec![0u8; READ_BUFFER],
            pos: 0,
            end: 0,
            gathered: Vec::new(),
            wait,
            way_out: "",
            aside: Aside::default(),
        }
    }

    /// A connection in the clear — `sslmode=disable`, and nothing else reaches this. `wait` is
    /// the read timeout already set on the socket, kept here only to be named in an error.
    pub fn plain(s: TcpStream, wait: Option<Duration>) -> Self {
        Stream::over(Raw::Plain(s), wait)
    }

    /// A connection under TLS, the handshake already driven.
    pub fn tls(s: StreamOwned<ClientConnection, TcpStream>, wait: Option<Duration>) -> Self {
        Stream::over(Raw::Tls(Box::new(s)), wait)
    }

    /// Change how long a read waits — `None` for as long as it takes — and what the error says
    /// can be done when it runs out. The handshake has one bound and a statement another.
    pub fn set_wait(&mut self, wait: Option<Duration>, way_out: &'static str) -> Result<(), String> {
        let socket = match &self.raw {
            Raw::Plain(s) => s,
            Raw::Tls(s) => &s.sock,
        };
        socket.set_read_timeout(wait).map_err(|e| format!("setting a read timeout: {e}"))?;
        self.wait = wait;
        self.way_out = way_out;
        Ok(())
    }

    /// The certificate the server presented, as DER — what a channel-bound authentication
    /// signs a hash of (`scram::end_point_hash`). `None` in the clear.
    pub fn peer_certificate(&self) -> Option<&[u8]> {
        match &self.raw {
            Raw::Plain(_) => None,
            Raw::Tls(s) => s.conn.peer_certificates().and_then(|chain| chain.first()).map(|c| c.as_ref()),
        }
    }

    /// Whether this connection is encrypted — for the diagnostic label, so a program that
    /// prints a connection says which kind it has.
    pub fn is_tls(&self) -> bool {
        matches!(self.raw, Raw::Tls(_))
    }

    /// Say goodbye at the TLS layer as well as the protocol one.
    ///
    /// Failure is ignored: this runs from `Drop`, which must not raise, and a session
    /// that cannot be closed politely is still closed when the descriptor goes.
    pub fn close_notify(&mut self) {
        if let Raw::Tls(s) = &mut self.raw {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }

    /// One read into the free end of the buffer.
    fn read_more(&mut self) -> Result<(), String> {
        let free = self.buf.get_mut(self.end..).unwrap_or(&mut []);
        self.end += take_in(&mut self.raw, &mut self.aside, free, self.wait, self.way_out)?;
        Ok(())
    }

    fn socket(&self) -> &TcpStream {
        match &self.raw {
            Raw::Plain(s) => s,
            Raw::Tls(s) => &s.sock,
        }
    }

    /// Send a request that may be ANSWERED BEFORE IT HAS ALL BEEN SENT, taking in what comes
    /// back while it goes out (the module note has the deadlock this exists to make
    /// impossible). The socket does not block for the length of the call and is put back the
    /// way it was, whatever happened.
    pub fn send_draining(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.socket().set_nonblocking(true).map_err(|e| format!("sending to the server: {e}"))?;
        let sent = self.pump(bytes);
        let restored = self.socket().set_nonblocking(false).map_err(|e| format!("sending to the server: {e}"));
        sent.and(restored)
    }

    /// Move bytes in both directions until the whole request is out.
    fn pump(&mut self, bytes: &[u8]) -> Result<(), String> {
        use std::io::ErrorKind::{Interrupted, WouldBlock};
        let sending = |e: std::io::Error| format!("sending to the server: {e}");
        let reading = |e: std::io::Error| format!("reading from the server: {e}");
        let closed = || "sending to the server: the connection closed".to_string();
        // Where what arrives is read before it is set aside — only made if anything does.
        let mut chunk: Vec<u8> = Vec::new();
        // How much of the request the kernel — or, under TLS, the record layer — has taken.
        let mut fed = 0usize;
        let mut quiet_since = Instant::now();
        loop {
            let mut moved = false;
            match &mut self.raw {
                Raw::Plain(s) => {
                    while fed < bytes.len() {
                        match s.write(bytes.get(fed..).unwrap_or(&[])) {
                            Ok(0) => return Err(closed()),
                            Ok(n) => {
                                fed += n;
                                moved = true;
                            }
                            Err(e) if e.kind() == WouldBlock => break,
                            Err(e) if e.kind() == Interrupted => continue,
                            Err(e) => return Err(sending(e)),
                        }
                    }
                    if fed == bytes.len() {
                        return Ok(());
                    }
                    // The kernel will not take the rest yet. The server may be waiting for
                    // THIS side to read: take in what has arrived.
                    chunk.resize(READ_BUFFER, 0);
                    loop {
                        match s.read(&mut chunk) {
                            Ok(0) => return Err(closed()),
                            Ok(n) => {
                                self.aside.bytes.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                                moved = true;
                            }
                            Err(e) if e.kind() == WouldBlock => break,
                            Err(e) if e.kind() == Interrupted => continue,
                            Err(e) => return Err(reading(e)),
                        }
                    }
                }
                Raw::Tls(t) => {
                    // Plaintext into the record layer, as much as it will hold…
                    while fed < bytes.len() {
                        match t.conn.writer().write(bytes.get(fed..).unwrap_or(&[])) {
                            Ok(0) => break,
                            Ok(n) => {
                                fed += n;
                                moved = true;
                            }
                            Err(e) => return Err(sending(e)),
                        }
                    }
                    // …and its records onto the socket, as many as the kernel will take.
                    while t.conn.wants_write() {
                        match t.conn.write_tls(&mut t.sock) {
                            Ok(0) => return Err(closed()),
                            Ok(_) => moved = true,
                            Err(e) if e.kind() == WouldBlock => break,
                            Err(e) if e.kind() == Interrupted => continue,
                            Err(e) => return Err(sending(e)),
                        }
                    }
                    if fed == bytes.len() && !t.conn.wants_write() {
                        return Ok(());
                    }
                    // Records in, and their plaintext set aside — the record layer holds only
                    // so much unread, and refuses more until it is taken.
                    chunk.resize(READ_BUFFER, 0);
                    loop {
                        match t.conn.read_tls(&mut t.sock) {
                            Ok(0) => return Err(closed()),
                            Ok(_) => moved = true,
                            Err(e) if e.kind() == WouldBlock => break,
                            Err(e) if e.kind() == Interrupted => continue,
                            Err(e) => return Err(reading(e)),
                        }
                        t.conn.process_new_packets().map_err(|e| format!("reading from the server: {e}"))?;
                        loop {
                            match t.conn.reader().read(&mut chunk) {
                                Ok(0) => return Err(closed()),
                                Ok(n) => self.aside.bytes.extend_from_slice(chunk.get(..n).unwrap_or(&[])),
                                Err(e) if e.kind() == WouldBlock => break,
                                Err(e) if e.kind() == Interrupted => continue,
                                Err(e) => return Err(reading(e)),
                            }
                        }
                    }
                }
            }
            if moved {
                quiet_since = Instant::now();
                continue;
            }
            // Neither direction moved. That is a server busy with the statements it already
            // has, and the wait for it is the same wait a read gets.
            if let Some(w) = self.wait
                && quiet_since.elapsed() >= w
            {
                return Err(format!("the server neither read nor answered for {} s{}", w.as_secs(), self.way_out));
            }
            wait_until_ready(self.socket());
        }
    }

    /// Make `n` unread bytes available in one piece. `n` never exceeds the buffer.
    fn fill(&mut self, n: usize) -> Result<(), String> {
        if self.pos == self.end {
            self.pos = 0;
            self.end = 0;
        }
        // No room for `n` bytes after `pos`: what is unread moves to the front.
        if self.pos + n > self.buf.len() {
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
        }
        while self.end - self.pos < n {
            self.read_more()?;
        }
        Ok(())
    }

    /// The next backend message: its type byte, and its body lent from the buffer it arrived
    /// in. The length is validated before it sizes anything (`proto::header`).
    pub fn next_msg(&mut self) -> Result<(u8, &[u8]), String> {
        if self.gathered.capacity() > KEEP_GATHERED {
            self.gathered = Vec::new();
        }
        self.fill(proto::HEADER)?;
        let head = self.buf.get(self.pos..self.pos + proto::HEADER).unwrap_or(&[]);
        let (tag, len) = proto::header(head)?;
        self.pos += proto::HEADER;

        if len <= self.buf.len() {
            self.fill(len)?;
            let body = self.buf.get(self.pos..self.pos + len).unwrap_or(&[]);
            self.pos += len;
            return Ok((tag, body));
        }

        // Larger than the buffer: what has arrived is copied once, and the rest is read
        // straight into place.
        self.gathered.clear();
        self.gathered.extend_from_slice(self.buf.get(self.pos..self.end).unwrap_or(&[]));
        self.pos = 0;
        self.end = 0;
        let mut at = self.gathered.len();
        self.gathered.resize(len, 0);
        while at < len {
            let rest = self.gathered.get_mut(at..).unwrap_or(&mut []);
            at += take_in(&mut self.raw, &mut self.aside, rest, self.wait, self.way_out)?;
        }
        Ok((tag, &self.gathered))
    }
}

/// Bytes from the server into `dst`: what a draining send set aside first, then the socket.
/// The one place a read happens, so nothing set aside can be read around.
fn take_in(
    raw: &mut Raw,
    aside: &mut Aside,
    dst: &mut [u8],
    wait: Option<Duration>,
    way_out: &str,
) -> Result<usize, String> {
    if let Some(n) = aside.serve(dst) {
        return Ok(n);
    }
    loop {
        match raw.read(dst) {
            Ok(0) => return Err("reading from the server: the connection closed".to_string()),
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(&e, wait, way_out)),
        }
    }
}

/// Sleep until the socket can be read or written, or a second has gone by — the caller looks
/// again either way, and keeps its own count of how long nothing has moved.
#[cfg(unix)]
fn wait_until_ready(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN | libc::POLLOUT, revents: 0 };
    // SAFETY: one initialised `pollfd` for a descriptor that is open for the whole call, and a
    // count of one — `poll`'s contract. Its answer is not needed: the caller retries both
    // directions whatever woke it.
    unsafe {
        libc::poll(&mut fd, 1, 1000);
    }
}

/// Without `poll`, a short sleep: the caller only comes here when the request is larger than
/// the socket will take and nothing has arrived, so this is never the common path.
#[cfg(not(unix))]
fn wait_until_ready(_: &TcpStream) {
    std::thread::sleep(Duration::from_millis(1));
}

/// A failed read, said the way a reader can act on. A timeout arrives as `WouldBlock` on Unix
/// and `TimedOut` on Windows, and "Resource temporarily unavailable (os error 11)" is not a
/// sentence anyone should have to decode.
fn read_error(e: &std::io::Error, wait: Option<Duration>, way_out: &str) -> String {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    match (e.kind(), wait) {
        (WouldBlock | TimedOut, Some(w)) => {
            format!("the server did not answer within {} s{way_out}", w.as_secs())
        }
        _ => format!("reading from the server: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A socket pair: what `serve` writes is what the `Stream` reads.
    fn fed_by(serve: impl FnOnce(TcpStream) + Send + 'static) -> Stream {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((c, _)) = l.accept() {
                let _ = c.set_nodelay(true);
                serve(c);
            }
        });
        Stream::plain(TcpStream::connect(addr).expect("connect"), None)
    }

    fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// Every message comes back whole and in order however the bytes were cut on the way in:
    /// many messages in one segment, one message across many, a header split from its body, a
    /// message that wraps the end of the buffer, and one larger than the buffer altogether.
    #[test]
    fn messages_survive_any_cut_of_the_bytes() {
        let sizes = [0usize, 1, 7, 300, READ_BUFFER - 5, READ_BUFFER - 4, 3, READ_BUFFER, 40_000, 2, 9_000, 9_000, 9_000, 0];
        let body_of = |i: usize, n: usize| (0..n).map(|k| ((k * 31 + i * 7) % 251) as u8).collect::<Vec<u8>>();
        let mut wire = Vec::new();
        for (i, n) in sizes.iter().enumerate() {
            wire.extend_from_slice(&framed(b'A' + (i % 20) as u8, &body_of(i, *n)));
        }
        for cut in [1usize, 3, 5, 6, 1000, 8192, 100_000] {
            let bytes = wire.clone();
            let mut s = fed_by(move |mut c| {
                for piece in bytes.chunks(cut) {
                    if c.write_all(piece).is_err() {
                        return;
                    }
                    let _ = c.flush();
                }
            });
            for (i, n) in sizes.iter().enumerate() {
                let (tag, body) = s.next_msg().unwrap_or_else(|e| panic!("cut {cut}, message {i}: {e}"));
                assert_eq!(tag, b'A' + (i % 20) as u8, "cut {cut}, message {i}");
                assert_eq!(body, body_of(i, *n).as_slice(), "cut {cut}, message {i} of {n} bytes");
            }
            // And when the server has nothing more to say, that is an error, not a hang.
            let e = s.next_msg().expect_err("the stream ended");
            assert!(e.contains("connection closed"), "{e}");
        }
    }

    /// THE DEADLOCK, AND THAT IT CANNOT FORM. The server here does what a real one does with a
    /// flight of many statements: it sends megabytes of answer BEFORE it has read the request,
    /// and will not read until it has. A client that writes first and reads afterwards never
    /// finishes writing — both sides wait for the other to read. `send_draining` takes the
    /// answer in while the request goes out, and then every message is there, in order.
    #[test]
    fn a_request_answered_before_it_is_all_sent_does_not_deadlock() {
        const MESSAGES: usize = 600;
        let body_of = |i: usize| (0..10_000).map(|k| ((k * 7 + i) % 251) as u8).collect::<Vec<u8>>();
        let request: Vec<u8> = (0..6_000_000usize).map(|k| (k % 253) as u8).collect();
        let expected = request.clone();
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().expect("accept");
            c.set_write_timeout(Some(Duration::from_secs(20))).expect("timeout");
            c.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
            for i in 0..MESSAGES {
                c.write_all(&framed(b'D', &body_of(i))).expect("the client is taking the answer in");
            }
            let mut got = vec![0u8; expected.len()];
            c.read_exact(&mut got).expect("the whole request arrives");
            assert!(got == expected, "the request arrived intact");
            c.write_all(&framed(b'Z', b"I")).expect("ready");
        });
        let tcp = TcpStream::connect(addr).expect("connect");
        let wait = Duration::from_secs(20);
        tcp.set_read_timeout(Some(wait)).expect("timeout");
        tcp.set_write_timeout(Some(wait)).expect("timeout");
        let mut s = Stream::plain(tcp, Some(wait));
        s.send_draining(&request).expect("sent without waiting on a server that is waiting on us");
        for i in 0..MESSAGES {
            let (tag, body) = s.next_msg().unwrap_or_else(|e| panic!("message {i}: {e}"));
            assert_eq!(tag, b'D');
            assert!(body == body_of(i).as_slice(), "message {i} is what was sent, in order");
        }
        assert_eq!(s.next_msg().expect("ready").0, b'Z');
        server.join().expect("the server finished");
    }

    /// A length the server made up never sizes a buffer: it is refused at the header.
    #[test]
    fn an_impossible_length_is_refused_before_anything_is_allocated() {
        for len in [-1i32, 0, 3, i32::MAX] {
            let mut s = fed_by(move |mut c| {
                let mut out = vec![b'D'];
                out.extend_from_slice(&len.to_be_bytes());
                let _ = c.write_all(&out);
            });
            let e = s.next_msg().expect_err("refused");
            assert!(e.contains("impossible length") || e.contains("over the"), "{len}: {e}");
        }
    }

    /// A server that stops talking mid-message is an error naming how long was waited.
    #[test]
    fn a_silent_server_is_a_timeout_that_says_so() {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let hold = std::thread::spawn(move || {
            let c = l.accept().map(|(c, _)| c);
            std::thread::sleep(Duration::from_millis(2600));
            drop(c);
        });
        let tcp = TcpStream::connect(addr).expect("connect");
        let wait = Duration::from_secs(1);
        tcp.set_read_timeout(Some(wait)).expect("timeout");
        let mut s = Stream::plain(tcp, Some(wait));
        let e = s.next_msg().expect_err("nothing arrives");
        assert_eq!(e, "the server did not answer within 1 s");
        // And when there is something a caller can do about it, the error says what.
        s.set_wait(Some(wait), " — and here is the way out").expect("set");
        let e = s.next_msg().expect_err("still nothing");
        assert_eq!(e, "the server did not answer within 1 s — and here is the way out");
        let _ = hold.join();
    }
}
