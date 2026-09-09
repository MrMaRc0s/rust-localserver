//! Per-client state machine, and the only place client socket I/O happens.
//!
//! # The one-syscall rule ([A-5])
//!
//! [`Connection::on_readable`] performs **exactly one** `read()` and
//! [`Connection::on_writable`] **exactly one** `write()`. Neither loops until
//! `WouldBlock`.
//!
//! Draining one client until it blocks would let a single fast uploader (or a
//! slow-loris dribbling bytes) hold the thread while every other client waits —
//! with one thread there is nobody else to serve them. One syscall per readiness
//! event gives fair round-robin service instead. Nothing is lost by stopping
//! early: the sockets are registered level-triggered, so if data remains the
//! next `poll()` reports the connection ready again immediately.
//!
//! # The error rule ([A-6], [A-7])
//!
//! Every I/O return value goes through [`classify_read`] / [`classify_write`],
//! which collapse it to an [`IoOutcome`]. The distinction that matters:
//! `WouldBlock` and `Interrupted` are **not errors** — they mean "nothing to do
//! right now", and treating them as failures (dropping the client) is the
//! classic bug in this project. A real error, and a `read()` of 0 bytes (peer
//! closed), both end the connection.
//!
//! # Closing and file descriptors ([A-33])
//!
//! A closed connection is removed from the server's map, which drops this struct
//! and closes the socket via `Drop`. There is no explicit `close()` to forget:
//! the fd's lifetime is the struct's lifetime.
//!
//! # Interest ([A-8])
//!
//! [`Connection::desired_interest`] returns `READABLE` **or** `WRITABLE`, never
//! both: a connection is either receiving a request or sending a response. The
//! server re-registers on every state change, so the epoll registration always
//! reflects what this connection is actually waiting for.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::{Interest, Token};

/// Bytes requested per `read()`. One syscall moves at most this much.
const READ_CHUNK: usize = 16 * 1024;

/// Cap on a request header block. Beyond this the request is refused rather
/// than buffered indefinitely — an unbounded header is a memory-exhaustion
/// vector, and no legitimate client needs more.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// No bytes moved for this long: the connection is idle.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A single request may not occupy a connection longer than this in total,
/// however much it dribbles. This is the backstop against a client that sends
/// one byte every 29 seconds forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What the server should do with a connection after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep it registered and wait for the next event.
    Keep,
    /// Deregister, close the socket, forget the connection.
    Close,
}

/// The result of one I/O syscall, normalised.
#[derive(Debug)]
pub enum IoOutcome {
    /// `n` bytes moved, `n > 0`.
    Progress(usize),
    /// `read()` returned 0: the peer performed an orderly shutdown.
    PeerClosed,
    /// Not an error. The socket is not ready; wait for the next event.
    WouldBlock,
    /// A real failure on this socket. The client must be dropped.
    Failed(io::Error),
}

/// Normalise the return value of a `read()`.
///
/// `Ok(0)` from a read means end-of-stream — the peer closed its side.
pub fn classify_read(result: io::Result<usize>) -> IoOutcome {
    match result {
        Ok(0) => IoOutcome::PeerClosed,
        Ok(n) => IoOutcome::Progress(n),
        // Neither of these is a failure: retry when the socket is ready again.
        Err(e) if e.kind() == ErrorKind::WouldBlock => IoOutcome::WouldBlock,
        Err(e) if e.kind() == ErrorKind::Interrupted => IoOutcome::WouldBlock,
        Err(e) => IoOutcome::Failed(e),
    }
}

/// Normalise the return value of a `write()`.
///
/// Unlike a read, `Ok(0)` here is not end-of-stream: it means the socket
/// accepted nothing while claiming to be writable. Treating it as progress
/// would spin the event loop, so it is reported as a failure.
pub fn classify_write(result: io::Result<usize>) -> IoOutcome {
    match result {
        Ok(0) => IoOutcome::Failed(io::Error::new(
            ErrorKind::WriteZero,
            "socket accepted no bytes while writable",
        )),
        Ok(n) => IoOutcome::Progress(n),
        Err(e) if e.kind() == ErrorKind::WouldBlock => IoOutcome::WouldBlock,
        Err(e) if e.kind() == ErrorKind::Interrupted => IoOutcome::WouldBlock,
        Err(e) => IoOutcome::Failed(e),
    }
}

/// Where a connection is in the request/response cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accumulating bytes until the end of the header block.
    ReadingHeaders,
    /// Header block parsed; accumulating the body. Entered by the request
    /// parser in TASKS.md 3.2.
    #[allow(dead_code)]
    ReadingBody,
    /// A response is queued; flushing it.
    Writing,
}

/// Why a connection has run out of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// Still within both deadlines.
    None,
    /// No bytes moved for [`IDLE_TIMEOUT`].
    Idle,
    /// Exceeded [`REQUEST_TIMEOUT`] regardless of activity.
    Hard,
}

pub struct Connection {
    pub socket: TcpStream,
    pub peer: SocketAddr,
    // Read back by the CGI plumbing in TASKS.md 7.1 to link a pipe event to
    // the connection that owns it.
    #[allow(dead_code)]
    pub token: Token,
    /// The interest currently registered with the poll, so the server can skip
    /// a `reregister` syscall when nothing changed.
    pub registered: Interest,
    state: State,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    /// How much of `write_buf` has been handed to the kernel so far.
    write_pos: usize,
    /// Close once the queued response has been flushed (`Connection: close`,
    /// or an error response that ends the conversation).
    close_after_write: bool,
    keep_alive: bool,
    created_at: Instant,
    last_activity: Instant,
    /// Requests answered on this connection; keep-alive reuse is visible here.
    pub requests: u32,
}

impl Connection {
    pub fn new(socket: TcpStream, peer: SocketAddr, token: Token) -> Connection {
        let now = Instant::now();
        Connection {
            socket,
            peer,
            token,
            registered: Interest::READABLE,
            state: State::ReadingHeaders,
            read_buf: Vec::with_capacity(READ_CHUNK),
            write_buf: Vec::new(),
            write_pos: 0,
            close_after_write: false,
            keep_alive: true,
            created_at: now,
            last_activity: now,
            requests: 0,
        }
    }

    // Used by the router in TASKS.md 4.x and by the metrics endpoint in 8.1.
    #[allow(dead_code)]
    pub fn state(&self) -> State {
        self.state
    }

    /// `READABLE` while receiving, `WRITABLE` while sending — never both. A
    /// connection is only ever waiting for one of the two.
    pub fn desired_interest(&self) -> Interest {
        match self.state {
            State::ReadingHeaders | State::ReadingBody => Interest::READABLE,
            State::Writing => Interest::WRITABLE,
        }
    }

    /// Exactly one `read()` syscall. No loop.
    fn read_once(&mut self) -> IoOutcome {
        let mut chunk = [0u8; READ_CHUNK];
        let outcome = classify_read(self.socket.read(&mut chunk));
        if let IoOutcome::Progress(n) = outcome {
            self.read_buf.extend_from_slice(&chunk[..n]);
            self.last_activity = Instant::now();
        }
        outcome
    }

    /// Exactly one `write()` syscall. No loop.
    fn write_once(&mut self) -> IoOutcome {
        let pending = &self.write_buf[self.write_pos..];
        if pending.is_empty() {
            return IoOutcome::Progress(0);
        }
        let outcome = classify_write(self.socket.write(pending));
        if let IoOutcome::Progress(n) = outcome {
            self.write_pos += n;
            self.last_activity = Instant::now();
        }
        outcome
    }

    /// Handle one readable event: one read, then advance the state machine over
    /// whatever is now buffered.
    pub fn on_readable(&mut self) -> Action {
        match self.read_once() {
            IoOutcome::Progress(_) => self.advance(),
            // The peer closed. If a response is still queued we cannot deliver
            // it, so there is nothing to do but drop the connection.
            IoOutcome::PeerClosed => Action::Close,
            IoOutcome::WouldBlock => Action::Keep,
            IoOutcome::Failed(e) => {
                eprintln!("localserver: read from {} failed: {e}", self.peer);
                Action::Close
            }
        }
    }

    /// Handle one writable event: one write, then decide whether the response
    /// is finished.
    pub fn on_writable(&mut self) -> Action {
        match self.write_once() {
            IoOutcome::Progress(_) => {
                if self.write_pos < self.write_buf.len() {
                    // Partially flushed; the next writable event continues.
                    return Action::Keep;
                }
                if self.close_after_write || !self.keep_alive {
                    Action::Close
                } else {
                    self.reset_for_next_request();
                    Action::Keep
                }
            }
            IoOutcome::WouldBlock => Action::Keep,
            IoOutcome::PeerClosed => Action::Close,
            IoOutcome::Failed(e) => {
                eprintln!("localserver: write to {} failed: {e}", self.peer);
                Action::Close
            }
        }
    }

    /// Advance after new bytes arrived.
    ///
    /// Phase 3 replaces the header scan and the canned reply with the real
    /// parser and router; the surrounding I/O contract does not change.
    fn advance(&mut self) -> Action {
        if self.state != State::ReadingHeaders {
            return Action::Keep;
        }
        if find_header_end(&self.read_buf).is_none() {
            if self.read_buf.len() > MAX_HEADER_BYTES {
                // Refuse rather than buffer without bound.
                self.queue_response(placeholder_error(431, "Request Header Fields Too Large"));
                self.close_after_write = true;
                return Action::Keep;
            }
            // Incomplete: wait for the next readable event.
            return Action::Keep;
        }

        self.keep_alive = wants_keep_alive(&self.read_buf);
        self.requests += 1;
        let response = placeholder_ok(self.keep_alive);
        self.queue_response(response);
        Action::Keep
    }

    /// Queue a response and switch to writing.
    fn queue_response(&mut self, bytes: Vec<u8>) {
        self.write_buf = bytes;
        self.write_pos = 0;
        self.state = State::Writing;
    }

    /// Response fully flushed and the connection is being kept alive: start
    /// over. Buffers are cleared rather than reallocated.
    fn reset_for_next_request(&mut self) {
        self.read_buf.clear();
        self.write_buf.clear();
        self.write_pos = 0;
        self.state = State::ReadingHeaders;
        self.close_after_write = false;
        self.last_activity = Instant::now();
    }

    /// Deadline check, run on every loop tick.
    pub fn timeout(&self, now: Instant) -> Timeout {
        if now.duration_since(self.created_at) > REQUEST_TIMEOUT {
            return Timeout::Hard;
        }
        if now.duration_since(self.last_activity) > IDLE_TIMEOUT {
            return Timeout::Idle;
        }
        Timeout::None
    }

    /// React to an idle timeout.
    ///
    /// An idle connection that has sent nothing is just an unused keep-alive
    /// slot: close it quietly, that is not an error. One that sent a partial
    /// request gets a 408 so the client learns why, then closes.
    pub fn on_idle_timeout(&mut self) -> Action {
        match self.state {
            State::ReadingHeaders | State::ReadingBody if self.read_buf.is_empty() => Action::Close,
            State::ReadingHeaders | State::ReadingBody => {
                self.queue_response(placeholder_error(408, "Request Timeout"));
                self.close_after_write = true;
                // Give the 408 its own window to flush instead of being
                // immediately re-reaped by the next sweep.
                self.last_activity = Instant::now();
                Action::Keep
            }
            // A response that cannot be flushed within the idle window is a
            // dead peer; there is nothing useful left to send.
            State::Writing => Action::Close,
        }
    }

    /// Seconds this connection has existed, for logging.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

/// Index just past the `\r\n\r\n` that ends a header block.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Cheap scan for `Connection: close`. Phase 3 replaces this with the parsed
/// header map; HTTP/1.1 defaults to keep-alive.
fn wants_keep_alive(buf: &[u8]) -> bool {
    let head = match find_header_end(buf) {
        Some(end) => &buf[..end],
        None => buf,
    };
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    // HTTP/1.0 defaults to closing after each response unless it explicitly
    // asks to keep the connection; HTTP/1.1 is the other way round.
    if text.contains("http/1.0") && !text.contains("connection: keep-alive") {
        return false;
    }
    !text.contains("connection: close")
}

/// Placeholder 200, replaced in Phase 3 by the router and response builder.
fn placeholder_ok(keep_alive: bool) -> Vec<u8> {
    let body = b"LocalServer: event loop online.\n";
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut out = format!(
        "HTTP/1.1 200 OK\r\n\
         Server: localserver\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: {connection}\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Placeholder error response, replaced in Phase 5.5 by the configured pages.
fn placeholder_error(status: u16, reason: &str) -> Vec<u8> {
    let body = format!("{status} {reason}\n");
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Server: localserver\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn read_of_zero_bytes_is_peer_closed() {
        assert!(matches!(classify_read(Ok(0)), IoOutcome::PeerClosed));
    }

    #[test]
    fn would_block_is_not_an_error() {
        // The classic bug this guards against: dropping a client because the
        // socket simply had nothing to give right now.
        let wb = io::Error::new(ErrorKind::WouldBlock, "");
        assert!(matches!(classify_read(Err(wb)), IoOutcome::WouldBlock));
        let wb = io::Error::new(ErrorKind::WouldBlock, "");
        assert!(matches!(classify_write(Err(wb)), IoOutcome::WouldBlock));
    }

    #[test]
    fn interrupted_is_retried_not_fatal() {
        let intr = io::Error::new(ErrorKind::Interrupted, "");
        assert!(matches!(classify_read(Err(intr)), IoOutcome::WouldBlock));
    }

    #[test]
    fn real_errors_are_fatal_for_the_client() {
        let reset = io::Error::new(ErrorKind::ConnectionReset, "");
        assert!(matches!(classify_read(Err(reset)), IoOutcome::Failed(_)));
    }

    /// A zero-length write while writable would spin the loop forever if it
    /// were treated as progress.
    #[test]
    fn zero_length_write_is_an_error_not_progress() {
        assert!(matches!(classify_write(Ok(0)), IoOutcome::Failed(_)));
    }

    #[test]
    fn finds_the_end_of_a_header_block() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
        // Body bytes after the terminator do not move the boundary.
        assert_eq!(find_header_end(b"A\r\n\r\nBODY"), Some(5));
    }

    #[test]
    fn detects_connection_close() {
        assert!(wants_keep_alive(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(!wants_keep_alive(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n"));
        // Header names and values are case-insensitive.
        assert!(!wants_keep_alive(b"GET / HTTP/1.1\r\nCONNECTION: CLOSE\r\n\r\n"));
        // HTTP/1.0 defaults to close unless it asks for keep-alive.
        assert!(!wants_keep_alive(b"GET / HTTP/1.0\r\nHost: x\r\n\r\n"));
        assert!(wants_keep_alive(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"));
    }

    #[test]
    fn placeholder_responses_are_well_formed() {
        let ok = placeholder_ok(true);
        let text = String::from_utf8_lossy(&ok);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 32\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        // Header block is properly terminated and the body follows it.
        let end = find_header_end(&ok).unwrap();
        assert_eq!(ok.len() - end, 32);
    }
}
