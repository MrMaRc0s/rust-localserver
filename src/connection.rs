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

use crate::config::Method;
use crate::http::request::{Progress, Request, RequestParser};
use crate::http::response::Response;

/// Bytes requested per `read()`. One syscall moves at most this much.
const READ_CHUNK: usize = 16 * 1024;

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
    /// Header block parsed; accumulating the body.
    ReadingBody,
    /// A response is queued; flushing it.
    Writing,
}

/// What to do once the queued bytes have been flushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterWrite {
    /// Response complete and the connection is not being reused.
    Close,
    /// Response complete; start reading the next request (keep-alive).
    NextRequest,
    /// That was an interim `100 Continue`; go back to reading this request's
    /// body. The response proper has not been sent yet.
    ResumeBody,
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
    /// Owns the bytes received but not yet consumed, so a request split across
    /// many reads survives between events.
    parser: RequestParser,
    write_buf: Vec<u8>,
    /// How much of `write_buf` has been handed to the kernel so far.
    write_pos: usize,
    after_write: AfterWrite,
    keep_alive: bool,
    /// A `100 Continue` has already been sent for the request in progress.
    sent_continue: bool,
    created_at: Instant,
    last_activity: Instant,
    /// Requests answered on this connection; keep-alive reuse is visible here.
    pub requests: u32,
}

impl Connection {
    /// `max_body` is the largest body any server on this listener accepts. The
    /// selected virtual host's own, stricter limit is applied once the `Host`
    /// header has been read (TASKS.md 4.1).
    pub fn new(socket: TcpStream, peer: SocketAddr, token: Token, max_body: usize) -> Connection {
        let now = Instant::now();
        Connection {
            socket,
            peer,
            token,
            registered: Interest::READABLE,
            state: State::ReadingHeaders,
            parser: RequestParser::new(max_body),
            write_buf: Vec::new(),
            write_pos: 0,
            after_write: AfterWrite::Close,
            keep_alive: true,
            sent_continue: false,
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
    ///
    /// The bytes go straight into the parser, which owns the buffering: a
    /// request split across many reads is its problem, not this one's.
    fn read_once(&mut self, chunk: &mut [u8]) -> IoOutcome {
        let outcome = classify_read(self.socket.read(chunk));
        if let IoOutcome::Progress(_) = outcome {
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
        let mut chunk = [0u8; READ_CHUNK];
        match self.read_once(&mut chunk) {
            IoOutcome::Progress(n) => self.process(&chunk[..n]),
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
                match self.after_write {
                    AfterWrite::Close => Action::Close,
                    AfterWrite::NextRequest if !self.keep_alive => Action::Close,
                    AfterWrite::NextRequest => {
                        self.reset_for_next_request();
                        // A pipelined request may already be buffered: drive the
                        // parser over it now rather than waiting for a read that
                        // may never come.
                        self.process(&[])
                    }
                    AfterWrite::ResumeBody => {
                        // The 100 Continue is out; carry on receiving the body.
                        self.write_buf.clear();
                        self.write_pos = 0;
                        self.state = State::ReadingBody;
                        self.process(&[])
                    }
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

    /// Drive the parser over `input` (which may be empty, to re-run it over
    /// bytes already buffered) and act on what it concludes.
    ///
    /// The three-way split is the point of the parser's design: "incomplete" is
    /// not an error, and must not close a client on a slow link.
    fn process(&mut self, input: &[u8]) -> Action {
        match self.parser.feed(input) {
            Progress::NeedMore => {
                if self.parser.headers_ready() {
                    self.state = State::ReadingBody;
                    // A client that announced `Expect: 100-continue` is waiting
                    // for permission before sending its body. Without the
                    // interim response it stalls for a second and then sends
                    // anyway, which would tax every upload.
                    if !self.sent_continue
                        && self.parser.head().map(|h| h.expects_continue()).unwrap_or(false)
                    {
                        self.sent_continue = true;
                        self.queue(b"HTTP/1.1 100 Continue\r\n\r\n".to_vec(), AfterWrite::ResumeBody);
                    }
                }
                Action::Keep
            }

            Progress::Complete => {
                let request = match self.parser.take() {
                    Some(r) => r,
                    // Cannot happen: Complete means a request is ready. Closing
                    // is the safe response to an impossible state.
                    None => return Action::Close,
                };
                self.sent_continue = false;
                self.keep_alive = request.wants_keep_alive();
                self.requests += 1;
                let response = respond(&request);
                let bytes = response.to_bytes(self.keep_alive);
                let next = if self.keep_alive { AfterWrite::NextRequest } else { AfterWrite::Close };
                self.queue(bytes, next);
                Action::Keep
            }

            Progress::Failed(status) => {
                // A malformed request leaves the byte stream in an unknown
                // state: there is no way to find where the next request would
                // begin, so the connection ends after the error response.
                self.keep_alive = false;
                let response = error_response(status);
                let bytes = response.to_bytes(false);
                self.queue(bytes, AfterWrite::Close);
                Action::Keep
            }
        }
    }

    /// Queue bytes for sending and switch to writing.
    fn queue(&mut self, bytes: Vec<u8>, after: AfterWrite) {
        self.write_buf = bytes;
        self.write_pos = 0;
        self.after_write = after;
        self.state = State::Writing;
    }

    /// Response fully flushed and the connection is being kept alive: start
    /// over. Buffers are cleared rather than reallocated.
    fn reset_for_next_request(&mut self) {
        self.write_buf.clear();
        self.write_pos = 0;
        self.state = State::ReadingHeaders;
        self.after_write = AfterWrite::Close;
        self.sent_continue = false;
        self.last_activity = Instant::now();
        // The parser keeps any pipelined bytes it has already received.
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
        let mid_request = self.parser.headers_ready() || self.parser.buffered() > 0;
        match self.state {
            State::ReadingHeaders | State::ReadingBody if !mid_request => Action::Close,
            State::ReadingHeaders | State::ReadingBody => {
                let bytes = error_response(408).to_bytes(false);
                self.keep_alive = false;
                self.queue(bytes, AfterWrite::Close);
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

/// Build the response for a parsed request.
///
/// Phase 3 stops here: the request is fully parsed and the status is chosen
/// correctly, but nothing is served from disk yet. TASKS.md 4.x replaces this
/// with virtual host selection and route matching, 5.x with the handlers.
fn respond(request: &Request) -> Response {
    let method = match request.method_enum() {
        Some(m) => m,
        // Well-formed but unimplemented: 501, not 400. The request is valid,
        // this server just does not do that.
        None => return error_response(501),
    };

    let summary = format!(
        "LocalServer\n\
         method:  {}\n\
         path:    {}\n\
         query:   {}\n\
         version: {}\n\
         host:    {}\n\
         headers: {}\n\
         body:    {} bytes\n",
        request.method,
        request.path,
        if request.query.is_empty() { "-" } else { &request.query },
        request.version.as_str(),
        request.host().unwrap_or_else(|| "-".to_string()),
        request.headers.len(),
        request.body.len(),
    );

    Response::with_body(200, "text/plain; charset=utf-8", summary.into_bytes())
        // A HEAD response reports the length a GET would have returned, but
        // sends no body.
        .head_only(method == Method::Head)
}

/// A minimal error response.
///
/// Replaced in TASKS.md 5.5 by the pages configured per server; this is the
/// built-in fallback that must always work, since an error page that itself
/// fails would recurse.
fn error_response(status: u16) -> Response {
    let reason = crate::http::response::reason_phrase(status);
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>{status} {reason}</title></head>\
         <body><h1>{status} {reason}</h1></body></html>\n"
    );
    Response::html(status, body).header("Connection", "close")
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
    fn an_unknown_method_is_not_implemented_rather_than_bad() {
        let mut parser = RequestParser::new(1024);
        assert_eq!(parser.feed(b"BREW /pot HTTP/1.1\r\nHost: x\r\n\r\n"), Progress::Complete);
        let request = parser.take().unwrap();
        // The request is well formed; the server just does not implement it.
        assert!(request.method_enum().is_none());
        assert_eq!(respond(&request).status, 501);
    }

    #[test]
    fn a_parsed_request_is_summarised_with_status_200() {
        let mut parser = RequestParser::new(1024);
        parser.feed(b"GET /a/b?x=1 HTTP/1.1\r\nHost: example.com\r\n\r\n");
        let request = parser.take().unwrap();
        let response = respond(&request);
        assert_eq!(response.status, 200);
        let text = String::from_utf8_lossy(&response.body).to_string();
        assert!(text.contains("path:    /a/b"), "{text}");
        assert!(text.contains("query:   x=1"), "{text}");
        assert!(text.contains("host:    example.com"), "{text}");
    }

    #[test]
    fn head_requests_get_a_length_but_no_body() {
        let mut parser = RequestParser::new(1024);
        parser.feed(b"HEAD / HTTP/1.1\r\nHost: x\r\n\r\n");
        let request = parser.take().unwrap();
        let bytes = respond(&request).to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Content-Length: "));
        assert!(!text.contains("Content-Length: 0\r\n"), "length is the body a GET would send");
        assert!(text.ends_with("\r\n\r\n"), "no body follows the headers");
    }

    #[test]
    fn error_responses_close_the_connection() {
        let response = error_response(400);
        assert_eq!(response.status, 400);
        assert_eq!(response.get_header("Connection"), Some("close"));
        let text = String::from_utf8_lossy(&response.to_bytes(false)).to_string();
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    }
}
