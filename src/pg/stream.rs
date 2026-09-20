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

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

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
        Stream { raw, buf: vec![0u8; READ_BUFFER], pos: 0, end: 0, gathered: Vec::new(), wait }
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

    /// One read from the socket into the free end of the buffer.
    fn read_more(&mut self) -> Result<(), String> {
        loop {
            let free = self.buf.get_mut(self.end..).unwrap_or(&mut []);
            match self.raw.read(free) {
                Ok(0) => return Err("reading from the server: the connection closed".to_string()),
                Ok(n) => {
                    self.end += n;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(&e, self.wait)),
            }
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
        let have = self.gathered.len();
        self.gathered.resize(len, 0);
        let mut at = have;
        while at < len {
            match self.raw.read(self.gathered.get_mut(at..).unwrap_or(&mut [])) {
                Ok(0) => return Err(format!("reading message '{}': the connection closed", tag as char)),
                Ok(n) => at += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read_error(&e, self.wait)),
            }
        }
        Ok((tag, &self.gathered))
    }
}

/// A failed read, said the way a reader can act on. A timeout arrives as `WouldBlock` on Unix
/// and `TimedOut` on Windows, and "Resource temporarily unavailable (os error 11)" is not a
/// sentence anyone should have to decode.
fn read_error(e: &std::io::Error, wait: Option<Duration>) -> String {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    match (e.kind(), wait) {
        (WouldBlock | TimedOut, Some(w)) => {
            format!("the server did not answer within {} s", w.as_secs())
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
            std::thread::sleep(Duration::from_millis(1500));
            drop(c);
        });
        let tcp = TcpStream::connect(addr).expect("connect");
        let wait = Duration::from_secs(1);
        tcp.set_read_timeout(Some(wait)).expect("timeout");
        let mut s = Stream::plain(tcp, Some(wait));
        let e = s.next_msg().expect_err("nothing arrives");
        assert_eq!(e, "the server did not answer within 1 s");
        let _ = hold.join();
    }
}
