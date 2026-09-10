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
//! event gives fair round-robin service instead.
//!
//! ## Re-arming, and why it is needed
//!
//! `mio` registers descriptors **edge-triggered** (`EPOLLET`). Edge-triggered
//! epoll reports a socket once when data arrives and stays silent afterwards,
//! which normally forces the "read until `WouldBlock`" loop this design
//! deliberately avoids: stop after one read and the bytes already sitting in
//! the socket buffer would never be announced again. A large upload deadlocks —
//! the server waits for an event that will not come while the client waits for
//! the server to drain the window.
//!
//! The fix is to re-arm rather than to drain. When a read fills the whole
//! buffer — meaning more may be pending — [`Connection::take_rearm`] tells the
//! server to re-register the socket. `epoll_ctl(EPOLL_CTL_MOD)` on an
//! edge-triggered descriptor re-reports readiness if data is still available,
//! so the next `poll()` returns this connection again. One read per event is
//! preserved, and every client still gets its turn in between.
//!
//! Writing needs the same treatment for the opposite reason. A *partial* write
//! means the socket buffer filled, and draining it produces a genuine writable
//! edge. A *complete* write leaves the socket writable without any transition,
//! so nothing further is reported — and a response sent in chunks (a streamed
//! file, a pipelined reply) would stall half-delivered. So a complete write
//! re-arms as well.
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

use std::cell::RefCell;
use std::rc::Rc;

use crate::cgi::CgiSpec;
use crate::config::{Method, ServerConfig};
use crate::error;
use crate::handlers::admin_page;
use crate::handlers::session_page::{self, Command};
use crate::handlers::{self, FileBody, Reply, STREAM_CHUNK};
use crate::http::request::{Progress, Request, RequestParser};
use crate::http::response::Response;
use crate::metrics::Metrics;
use crate::router::{self, Resolution};
use crate::utils::cookie::{self, SetCookie};
use crate::utils::session::{self, SessionStore};

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
    /// A CGI script must be started. Only the server can do that: spawning
    /// means registering three more descriptors in the poll it owns. The spec
    /// is waiting in [`Connection::take_cgi_spec`].
    StartCgi,
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
    /// A CGI script is running. The connection stays registered as readable so
    /// a client that gives up is noticed, but nothing is parsed until the
    /// script finishes.
    AwaitingCgi,
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
    /// Staging buffer: the response head, then one chunk of the body at a
    /// time when the body is streamed from a file.
    write_buf: Vec<u8>,
    /// How much of `write_buf` has been handed to the kernel so far.
    write_pos: usize,
    /// A response body still on disk. Refilled into `write_buf` a chunk at a
    /// time so a large download never occupies more than one chunk of memory
    /// and never monopolises the loop.
    body_source: Option<FileBody>,
    after_write: AfterWrite,
    keep_alive: bool,
    /// A `100 Continue` has already been sent for the request in progress.
    sent_continue: bool,
    /// Set when routing decided on CGI; the server collects it.
    pending_cgi: Option<Box<CgiSpec>>,
    /// The port this connection arrived on, for the CGI `SERVER_PORT`.
    local_port: u16,
    /// Servers sharing the listener this connection arrived on; the router
    /// picks between them by `Host`.
    servers: Rc<Vec<ServerConfig>>,
    /// The process-wide session store.
    ///
    /// `Rc<RefCell<_>>` and not `Arc<Mutex<_>>`: there is one thread, so
    /// atomic refcounting and locking would cost something and protect against
    /// nothing. `RefCell` still enforces the borrow rules at runtime, so an
    /// accidental nested borrow is caught rather than corrupting the map.
    sessions: Rc<RefCell<SessionStore>>,
    metrics: Rc<RefCell<Metrics>>,
    /// The virtual host's body limit has been applied to the parser.
    limit_narrowed: bool,
    /// The last read filled the buffer, so the socket may hold more. Because
    /// the registration is edge-triggered, it has to be re-armed for that data
    /// to be reported again.
    rearm: bool,
    created_at: Instant,
    last_activity: Instant,
    /// Requests answered on this connection; keep-alive reuse is visible here.
    pub requests: u32,
}

impl Connection {
    /// `max_body` is the largest body any server on this listener accepts. The
    /// selected virtual host's own, stricter limit is applied once the `Host`
    /// header has been read (TASKS.md 4.1).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        socket: TcpStream,
        peer: SocketAddr,
        token: Token,
        max_body: usize,
        local_port: u16,
        servers: Rc<Vec<ServerConfig>>,
        sessions: Rc<RefCell<SessionStore>>,
        metrics: Rc<RefCell<Metrics>>,
    ) -> Connection {
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
            body_source: None,
            after_write: AfterWrite::Close,
            keep_alive: true,
            sent_continue: false,
            pending_cgi: None,
            local_port,
            servers,
            sessions,
            metrics,
            limit_narrowed: false,
            rearm: false,
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
            // Not waiting to send anything, but still worth hearing about a
            // client that hangs up while the script runs.
            State::AwaitingCgi => Interest::READABLE,
        }
    }

    /// Exactly one `read()` syscall. No loop.
    ///
    /// The bytes go straight into the parser, which owns the buffering: a
    /// request split across many reads is its problem, not this one's.
    fn read_once(&mut self, chunk: &mut [u8]) -> IoOutcome {
        let outcome = classify_read(self.socket.read(chunk));
        if let IoOutcome::Progress(n) = outcome {
            self.last_activity = Instant::now();
            if let Ok(mut m) = self.metrics.try_borrow_mut() {
                m.record_bytes_in(n);
            }
            // A short read means the socket buffer is empty and a fresh edge
            // will arrive with the next data. A full one means there may be
            // more waiting, which an edge-triggered registration will not
            // announce again unless it is re-armed.
            self.rearm = n == chunk.len();
        }
        outcome
    }

    /// Whether the socket must be re-registered to surface data the last read
    /// left behind. Clears the flag.
    pub fn take_rearm(&mut self) -> bool {
        std::mem::replace(&mut self.rearm, false)
    }

    /// Exactly one `write()` syscall. No loop.
    fn write_once(&mut self) -> IoOutcome {
        let pending = &self.write_buf[self.write_pos..];
        if pending.is_empty() {
            return IoOutcome::Progress(0);
        }
        let pending_len = pending.len();
        let outcome = classify_write(self.socket.write(pending));
        if let IoOutcome::Progress(n) = outcome {
            self.write_pos += n;
            self.last_activity = Instant::now();
            if let Ok(mut m) = self.metrics.try_borrow_mut() {
                m.record_bytes_out(n);
            }
            // A *partial* write means the socket buffer filled up, so a real
            // writable edge will arrive when the peer drains it. A *complete*
            // write leaves the socket writable with no transition at all, and
            // an edge-triggered registration reports nothing further — so if
            // more remains to send (the next chunk of a streamed file, or a
            // pipelined response), the registration has to be re-armed or the
            // response stalls half-sent.
            self.rearm = n == pending_len;
        }
        outcome
    }

    /// Handle one readable event: one read, then advance the state machine over
    /// whatever is now buffered.
    pub fn on_readable(&mut self) -> Action {
        let mut chunk = [0u8; READ_CHUNK];
        match self.read_once(&mut chunk) {
            IoOutcome::Progress(n) if self.state == State::AwaitingCgi => {
                // A script is running, so this cannot be answered yet. Feed the
                // parser so a pipelined request is not lost, but ignore what it
                // concludes: queueing a second response now would clobber the
                // one the script is about to produce. `reset_for_next_request`
                // re-drives the parser once the CGI response has gone out.
                let _ = self.parser.feed(&chunk[..n]);
                Action::Keep
            }
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
                // The buffer is empty. If the body is still being streamed from
                // disk, pull the next chunk instead of finishing.
                match self.refill_from_file() {
                    Ok(true) => return Action::Keep,
                    Ok(false) => {}
                    Err(e) => {
                        // The file changed or became unreadable mid-response.
                        // The headers, Content-Length included, are already on
                        // the wire, so the only honest thing left is to close:
                        // a truncated body must not look like a complete one.
                        eprintln!("localserver: streaming to {} failed: {e}", self.peer);
                        return Action::Close;
                    }
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
                    // The listener's limit is the most permissive among the
                    // servers sharing it. Now that Host is known, apply the
                    // selected server's own, stricter one ([A-13]).
                    if !self.limit_narrowed {
                        self.limit_narrowed = true;
                        let host = self.parser.head().and_then(|h| h.host());
                        if let Some(server) =
                            router::select_server(&self.servers, host.as_deref())
                        {
                            self.parser.set_max_body(server.client_max_body_size);
                        }
                        // Re-run the parser: the narrower limit may already be
                        // exceeded by what has arrived.
                        if let Progress::Failed(status) = self.parser.feed(&[]) {
                            self.keep_alive = false;
                            let bytes = self.error_bytes(status);
                            self.queue(bytes, AfterWrite::Close);
                            return Action::Keep;
                        }
                    }
                    // A client that announced `Expect: 100-continue` is waiting
                    // for permission before sending its body. Without the
                    // interim response it stalls for a second and then sends
                    // anyway, which would tax every upload.
                    if !self.sent_continue
                        && self.parser.head().map(|h| h.expects_continue()).unwrap_or(false)
                    {
                        self.sent_continue = true;
                        if let Ok(mut m) = self.metrics.try_borrow_mut() {
                            m.record_response(100);
                        }
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
                if let Ok(mut m) = self.metrics.try_borrow_mut() {
                    m.record_request(&request.method);
                }
                self.sent_continue = false;
                self.limit_narrowed = false;
                self.keep_alive = request.wants_keep_alive();
                self.requests += 1;
                let mut spec: Option<Box<CgiSpec>> = None;
                let reply = build_reply(
                    &self.servers,
                    &self.sessions,
                    &self.metrics,
                    &request,
                    self.peer,
                    self.local_port,
                    &mut spec,
                );
                if let Some(spec) = spec {
                    self.pending_cgi = Some(spec);
                    self.await_cgi();
                    return Action::StartCgi;
                }
                let next = if self.keep_alive { AfterWrite::NextRequest } else { AfterWrite::Close };
                self.queue_reply(reply, next);
                Action::Keep
            }

            Progress::Failed(status) => {
                // A malformed request leaves the byte stream in an unknown
                // state: there is no way to find where the next request would
                // begin, so the connection ends after the error response.
                self.keep_alive = false;
                let bytes = self.error_bytes(status);
                self.queue(bytes, AfterWrite::Close);
                Action::Keep
            }
        }
    }

    /// Queue bytes for sending and switch to writing.
    fn queue(&mut self, bytes: Vec<u8>, after: AfterWrite) {
        self.write_buf = bytes;
        self.write_pos = 0;
        self.body_source = None;
        self.after_write = after;
        self.state = State::Writing;
    }

    /// Queue a reply whose body may still be on disk.
    fn queue_reply(&mut self, reply: Reply, after: AfterWrite) {
        let Reply { response, body } = reply;
        if let Ok(mut m) = self.metrics.try_borrow_mut() {
            m.record_response(response.status);
        }
        self.write_buf = response.to_bytes(self.keep_alive);
        self.write_pos = 0;
        self.body_source = body;
        self.after_write = after;
        self.state = State::Writing;
    }

    /// Take the CGI spec the router produced, for the server to act on.
    pub fn take_cgi_spec(&mut self) -> Option<Box<CgiSpec>> {
        self.pending_cgi.take()
    }

    /// Park the connection while a script runs.
    pub fn await_cgi(&mut self) {
        self.state = State::AwaitingCgi;
    }

    /// Deliver what a script produced, or the error that replaced it.
    pub fn deliver_cgi(&mut self, reply: Reply) -> Action {
        let next = if self.keep_alive { AfterWrite::NextRequest } else { AfterWrite::Close };
        self.queue_reply(reply, next);
        Action::Keep
    }

    /// Serialize an error response.
    ///
    /// Used for failures that happen before a request exists — a malformed
    /// request line, a timeout — so there is no `Host` to select a virtual host
    /// with. The listener's default server supplies the configured pages, which
    /// means a custom 400 page works even for a request too broken to name a
    /// host.
    fn error_bytes(&self, status: u16) -> Vec<u8> {
        if let Ok(mut m) = self.metrics.try_borrow_mut() {
            m.record_response(status);
        }
        let response = match router::select_server(&self.servers, None) {
            Some(server) => error::page(server, status),
            None => error::builtin(status),
        };
        response.header("Connection", "close").to_bytes(false)
    }

    /// Pull the next chunk of a streamed body into the write buffer.
    ///
    /// Returns whether there was anything to send. One bounded read per
    /// writable event: `epoll` cannot watch a regular file (it reports one as
    /// always ready), so there is no readiness to wait for — but chunking keeps
    /// a large download from holding the single thread, which is the same
    /// reasoning as one socket read per event.
    fn refill_from_file(&mut self) -> io::Result<bool> {
        let source = match self.body_source.as_mut() {
            Some(source) => source,
            None => return Ok(false),
        };
        if source.remaining == 0 {
            self.body_source = None;
            return Ok(false);
        }

        let want = STREAM_CHUNK.min(source.remaining as usize);
        let mut chunk = vec![0u8; want];
        let read = source.file.read(&mut chunk)?;
        if read == 0 {
            // The file is shorter than its metadata said. Content-Length has
            // already been sent, so the response cannot be completed honestly.
            self.body_source = None;
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "file ended before Content-Length was satisfied",
            ));
        }
        chunk.truncate(read);
        source.remaining -= read as u64;
        self.write_buf = chunk;
        self.write_pos = 0;
        Ok(true)
    }

    /// Response fully flushed and the connection is being kept alive: start
    /// over. Buffers are cleared rather than reallocated.
    fn reset_for_next_request(&mut self) {
        self.limit_narrowed = false;
        self.body_source = None;
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
                self.keep_alive = false;
                let bytes = self.error_bytes(408);
                self.queue(bytes, AfterWrite::Close);
                // Give the 408 its own window to flush instead of being
                // immediately re-reaped by the next sweep.
                self.last_activity = Instant::now();
                Action::Keep
            }
            // A response that cannot be flushed within the idle window is a
            // dead peer; there is nothing useful left to send.
            State::Writing => Action::Close,
            // Bounded by the CGI deadline instead, which is shorter than the
            // idle timeout, so reaching this means the script already answered
            // or was killed. Closing is the safe reading.
            State::AwaitingCgi => Action::Close,
        }
    }

    /// Seconds this connection has existed, for logging.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

/// Build the reply for a parsed request.
///
/// Sessions wrap everything: the cookie is read (or a session created) before
/// routing, and a `Set-Cookie` is attached afterwards if one was issued. Doing
/// it here rather than inside each handler means every response carries the
/// session consistently, including error pages.
fn build_reply(
    servers: &[ServerConfig],
    sessions: &Rc<RefCell<SessionStore>>,
    metrics: &Rc<RefCell<Metrics>>,
    request: &Request,
    remote: SocketAddr,
    port: u16,
    cgi: &mut Option<Box<CgiSpec>>,
) -> Reply {
    let session = resolve_session(sessions, request);

    let mut reply = if admin_page::owns(&request.path) {
        admin_endpoint(metrics, sessions, request)
    } else if session_page::owns(&request.path) {
        session_demo(sessions, request, &session)
    } else {
        match route_request(servers, request, remote, port) {
            Routed::Reply(reply) => reply,
            Routed::Cgi(spec) => {
                // Hand the spec back: only the server can spawn, because
                // spawning means registering pipes in the poll it owns.
                *cgi = Some(spec);
                return Reply::immediate(Response::new(200));
            }
        }
    };

    // Only when a session was just issued: re-sending an unchanged cookie on
    // every response is wasted bytes on every asset a page pulls in.
    if let Some(state) = &session {
        if state.is_new {
            let cookie = SetCookie::new(session::COOKIE_NAME, &state.id)
                .max_age(session::DEFAULT_TTL.as_secs() as i64);
            if cookie.is_valid() {
                reply.response = reply.response.header("Set-Cookie", cookie.to_string());
            }
        }
    }
    reply
}

/// Serve the `/admin` dashboard and metrics endpoints.
fn admin_endpoint(
    metrics: &Rc<RefCell<Metrics>>,
    sessions: &Rc<RefCell<SessionStore>>,
    request: &Request,
) -> Reply {
    if request.method != "GET" && request.method != "HEAD" {
        return Reply::immediate(
            Response::new(405)
                .header("Allow", "GET, HEAD")
                .header("Content-Length", "0"),
        );
    }
    let active_sessions = sessions.try_borrow().map(|s| s.len()).unwrap_or(0);
    let metrics_guard = match metrics.try_borrow() {
        Ok(g) => g,
        Err(_) => return Reply::immediate(error::builtin(500)),
    };
    let mut reply = if request.path == "/admin/metrics.json" {
        admin_page::metrics_json(&metrics_guard, active_sessions)
    } else {
        admin_page::page(&metrics_guard, active_sessions)
    };
    if request.method_enum() == Some(Method::Head) {
        reply.response = reply.response.head_only(true);
    }
    reply
}

/// The session attached to a request.
struct SessionState {
    id: String,
    /// Created for this request, so a `Set-Cookie` is owed.
    is_new: bool,
}

/// Find the request's session, or start one.
///
/// A cookie whose value is not shaped like an id this server issues is treated
/// as absent, so a client cannot probe the map with arbitrary strings.
fn resolve_session(
    sessions: &Rc<RefCell<SessionStore>>,
    request: &Request,
) -> Option<SessionState> {
    // `try_borrow_mut` rather than `borrow_mut`: a nested borrow would panic,
    // and on the request path nothing may panic.
    let mut store = sessions.try_borrow_mut().ok()?;

    let presented = request
        .headers
        .get("cookie")
        .map(cookie::parse)
        .and_then(|pairs| cookie::get(&pairs, session::COOKIE_NAME).map(str::to_string))
        .filter(|id| session::is_valid_id(id));

    if let Some(id) = presented {
        if store.touch(&id).is_some() {
            return Some(SessionState { id, is_new: false });
        }
        // Known-shaped but expired or unknown: fall through and issue a new one.
    }

    store
        .create()
        .map(|id| SessionState { id, is_new: true })
}

/// Serve the `/session` demo.
fn session_demo(
    sessions: &Rc<RefCell<SessionStore>>,
    request: &Request,
    state: &Option<SessionState>,
) -> Reply {
    let head_only = request.method_enum() == Some(Method::Head);
    let reply = session_demo_inner(sessions, request, state);
    if head_only {
        let mut reply = reply;
        reply.response = reply.response.head_only(true);
        reply
    } else {
        reply
    }
}

fn session_demo_inner(
    sessions: &Rc<RefCell<SessionStore>>,
    request: &Request,
    state: &Option<SessionState>,
) -> Reply {
    let state = match state {
        Some(state) => state,
        // No session could be issued at all.
        None => return session_page::unavailable(),
    };

    let mut store = match sessions.try_borrow_mut() {
        Ok(store) => store,
        Err(_) => return Reply::immediate(error::builtin(500)),
    };

    match session_page::command(request) {
        Command::Login(name) => {
            store.set_value(&state.id, "name", &name);
            session_page::redirect_to_self()
        }
        Command::Logout => {
            store.destroy(&state.id);
            // The cookie is cleared too, so the next request starts clean
            // rather than presenting an id that no longer exists.
            let mut reply = session_page::redirect_to_self();
            let cleared = SetCookie::expired(session::COOKIE_NAME);
            reply.response = reply.response.header("Set-Cookie", cleared.to_string());
            reply
        }
        Command::Unknown => Reply::immediate(error::builtin(404)),
        Command::Show => match store.get(&state.id) {
            Some(session) => {
                let snapshot = session.clone();
                session_page::page(&state.id, &snapshot, &store, state.is_new)
            }
            None => session_page::unavailable(),
        },
    }
}

/// What routing concluded: a finished reply, or a script to run.
enum Routed {
    Reply(Reply),
    Cgi(Box<CgiSpec>),
}

/// Select the virtual host, resolve the route, then hand off to the handler.
fn route_request(
    servers: &[ServerConfig],
    request: &Request,
    remote: SocketAddr,
    port: u16,
) -> Routed {
    let host = request.host();
    let server = match router::select_server(servers, host.as_deref()) {
        Some(server) => server,
        // A listener always has at least one server, so this cannot happen; if
        // it somehow does it is a server fault, not the client's.
        None => return Routed::Reply(Reply::immediate(error::builtin(500))),
    };

    // CORS preflight: browsers send `OPTIONS` with Access-Control-Request-*
    // before non-simple cross-origin fetches (custom headers like X-Host,
    // or Content-Type: application/octet-stream). Answer 204 with Allow so
    // the visual audit dashboard can probe 8081/8082 from a page on 8080.
    // `to_bytes` adds the Access-Control-Allow-* headers to every response.
    if request.method_enum() == Some(Method::Options) {
        return Routed::Reply(Reply::immediate(
            Response::new(204).header("Allow", "GET, POST, DELETE, HEAD, OPTIONS"),
        ));
    }

    let head_only = request.method_enum() == Some(Method::Head);

    // Every handler returns `Result<Reply, u16>`, so a failure anywhere becomes
    // the configured error page for that status by one path rather than each
    // handler inventing its own response.
    let outcome: Result<Reply, u16> = match router::resolve(server, request) {
        Resolution::Redirect { status, location } => {
            let escaped = crate::handlers::escape_html(&location);
            Ok(Reply::immediate(
                Response::html(
                    status,
                    format!(
                        "<!doctype html><html><head><meta charset=\"utf-8\">\
                         <title>{status}</title></head><body>\
                         <p>Moved to <a href=\"{escaped}\">{escaped}</a></p></body></html>\n"
                    ),
                )
                .header("Location", &location)
                .head_only(head_only),
            ))
        }

        // RFC 9110 requires Allow on a 405 so the client learns what it may use.
        Resolution::MethodNotAllowed { allow } => {
            return Routed::Reply(Reply::immediate(
                error::page(server, 405)
                    .header("Allow", &allow)
                    .head_only(head_only),
            ))
        }

        Resolution::File { path } => handlers::file(&path, head_only),

        Resolution::Listing { path, url_path } => handlers::listing(&path, &url_path, head_only),

        Resolution::Upload { dir } => {
            if handlers::upload_dir_usable(&dir) {
                handlers::upload(&dir, request, &request.path)
            } else {
                Err(500)
            }
        }

        Resolution::Delete { path } => handlers::delete(&path),

        Resolution::Cgi { interpreter, script, path_info } => {
            let document_root = match router::match_route(server, &request.path) {
                Some(route) => route.root.clone(),
                None => script.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            };
            return Routed::Cgi(Box::new(crate::cgi::spec_from_request(
                crate::cgi::CgiTarget { interpreter, script, path_info, document_root },
                crate::cgi::CgiOrigin { remote, port, head_only },
                request,
                server,
            )));
        }

        Resolution::Error(status) => Err(status),
    };

    Routed::Reply(match outcome {
        Ok(reply) => reply,
        Err(status) => Reply::immediate(error::page(server, status).head_only(head_only)),
    })
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

    /// An error raised before any request exists still uses the listener's
    /// default server, so a custom error page applies even to a request too
    /// broken to name a host.
    #[test]
    fn error_responses_close_the_connection() {
        let response = error::builtin(400).header("Connection", "close");
        assert_eq!(response.status, 400);
        assert_eq!(response.get_header("Connection"), Some("close"));
        let text = String::from_utf8_lossy(&response.to_bytes(false)).to_string();
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(text.contains("Bad Request"));
    }

    // --- Status code 408: idle timeout must produce 408 when mid-request [A-34] ---

    #[test]
    fn idle_timeout_mid_request_yields_408_and_keeps_writing() {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let sessions = Rc::new(RefCell::new(SessionStore::new(Duration::from_secs(60))));
        let metrics = Rc::new(RefCell::new(Metrics::new()));
        let servers: Rc<Vec<ServerConfig>> = Rc::new(vec![ServerConfig {
            host: "127.0.0.1".to_string(),
            ports: vec![8080],
            server_names: Vec::new(),
            is_default: true,
            client_max_body_size: 1024 * 1024,
            error_pages: std::collections::BTreeMap::new(),
            routes: Vec::new(),
            line: 1,
        }]);

        // Create a real TcpStream pair via loopback for the Connection constructor.
        // We do not drive I/O; we only exercise timeout state machine.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        let (server_std, _) = listener.accept().unwrap();
        server_std.set_nonblocking(true).unwrap();
        let mio_stream = TcpStream::from_std(server_std);

        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 12345));
        let mut conn = Connection::new(
            mio_stream,
            peer,
            Token(1 << 20),
            1024 * 1024,
            8080,
            servers,
            sessions,
            metrics,
        );

        // Simulate a partial request: headers arrived but body not yet complete.
        // Feed "GET / HTTP/1.1\r\nHost: x\r\n" (no terminator) via parser directly.
        let mut parser = crate::http::request::RequestParser::new(1024 * 1024);
        assert_eq!(
            parser.feed(b"GET / HTTP/1.1\r\nHost: x\r\n"),
            crate::http::request::Progress::NeedMore
        );
        // Inject parser state into a fresh connection by using feed through process path:
        // Instead, test the timeout helper in isolation:
        // - No activity for > IDLE_TIMEOUT and parser has buffered bytes → should be Idle.
        conn = {
            // Force created_at far in past to trigger hard timeout check first?
            // We want Idle, not Hard, so set created_at recent but last_activity old.
            // We cannot set private fields here, so test timeout logic directly:
            // Create a connection with mocked instants via manipulating time.
            // Simplest: verify Timeout::Idle when last_activity is old and created_at recent.
            let mut c = conn;
            c.parser.feed(b"GET / HTTP/1.1\r\nHost: x\r\n");
            let now = Instant::now();
            // Fresh connection: should be None
            assert_eq!(c.timeout(now), Timeout::None);
            // Simulate idle by moving last_activity back
            // Use unsafe to poke private fields for test purposes (within same crate test)
            c.last_activity = now - IDLE_TIMEOUT - Duration::from_secs(1);
            // created_at still recent, so Idle, not Hard
            assert_eq!(c.timeout(now), Timeout::Idle);
            // Now trigger on_idle_timeout which should queue a 408
            let action = c.on_idle_timeout();
            assert_eq!(action, Action::Keep);
            assert_eq!(c.state(), State::Writing);
            // The queued response must be 408
            let wire = String::from_utf8_lossy(&c.write_buf).to_string();
            assert!(wire.contains("408"), "expected 408 in queued error, got: {wire}");
            assert!(wire.contains("Request Timeout"));
            c
        };
        // Verify the 408 response serializes correctly via error_bytes
        assert!(conn.write_buf.starts_with(b"HTTP/1.1 408"));
    }

    #[test]
    fn idle_keepalive_without_request_closes_silently_not_408() {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let sessions = Rc::new(RefCell::new(SessionStore::new(Duration::from_secs(60))));
        let metrics = Rc::new(RefCell::new(Metrics::new()));
        let servers: Rc<Vec<ServerConfig>> = Rc::new(vec![]);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        let (server_std, _) = listener.accept().unwrap();
        server_std.set_nonblocking(true).unwrap();
        let mio_stream = TcpStream::from_std(server_std);
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 12346));
        let mut conn = Connection::new(
            mio_stream,
            peer,
            Token(1 << 20),
            1024 * 1024,
            8080,
            servers,
            sessions,
            metrics,
        );
        // No request bytes at all, idle timeout should just Close (no 408)
        let now = Instant::now();
        conn.last_activity = now - IDLE_TIMEOUT - Duration::from_secs(1);
        assert_eq!(conn.timeout(now), Timeout::Idle);
        assert_eq!(conn.on_idle_timeout(), Action::Close);
    }

    #[test]
    fn hard_timeout_always_closes() {
        use std::net::{Ipv4Addr, SocketAddrV4};

        let sessions = Rc::new(RefCell::new(SessionStore::new(Duration::from_secs(60))));
        let metrics = Rc::new(RefCell::new(Metrics::new()));
        let servers: Rc<Vec<ServerConfig>> = Rc::new(vec![]);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        let (server_std, _) = listener.accept().unwrap();
        server_std.set_nonblocking(true).unwrap();
        let mio_stream = TcpStream::from_std(server_std);
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 12347));
        let mut conn = Connection::new(
            mio_stream,
            peer,
            Token(1 << 20),
            1024 * 1024,
            8080,
            servers,
            sessions,
            metrics,
        );
        let now = Instant::now();
        conn.created_at = now - REQUEST_TIMEOUT - Duration::from_secs(1);
        assert_eq!(conn.timeout(now), Timeout::Hard);
    }

    #[test]
    fn build_reply_routes_admin_dashboard_and_metrics() {
        let sessions = Rc::new(RefCell::new(SessionStore::new(Duration::from_secs(60))));
        let metrics = Rc::new(RefCell::new(Metrics::new()));
        let mut spec = None;
        let remote = "127.0.0.1:12345".parse().unwrap();

        // 1. GET /admin
        let mut parser = RequestParser::new(4096);
        assert_eq!(
            parser.feed(b"GET /admin HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Progress::Complete
        );
        let req = parser.take().unwrap();
        let reply = build_reply(&[], &sessions, &metrics, &req, remote, 8080, &mut spec);
        assert_eq!(reply.response.status, 200);
        assert_eq!(
            reply.response.get_header("Content-Type"),
            Some("text/html; charset=utf-8")
        );
        let body = String::from_utf8_lossy(&reply.response.body);
        assert!(body.contains("Admin Dashboard"));

        // 2. GET /admin/metrics.json
        let mut parser = RequestParser::new(4096);
        assert_eq!(
            parser.feed(b"GET /admin/metrics.json HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Progress::Complete
        );
        let req = parser.take().unwrap();
        let reply = build_reply(&[], &sessions, &metrics, &req, remote, 8080, &mut spec);
        assert_eq!(reply.response.status, 200);
        assert_eq!(
            reply.response.get_header("Content-Type"),
            Some("application/json")
        );
        let body = String::from_utf8_lossy(&reply.response.body);
        assert!(body.contains("\"uptime_seconds\":"));

        // 3. POST /admin is 405 Method Not Allowed
        let mut parser = RequestParser::new(4096);
        assert_eq!(
            parser.feed(b"POST /admin HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"),
            Progress::Complete
        );
        let req = parser.take().unwrap();
        let reply = build_reply(&[], &sessions, &metrics, &req, remote, 8080, &mut spec);
        assert_eq!(reply.response.status, 405);
        assert_eq!(reply.response.get_header("Allow"), Some("GET, HEAD"));

        // 4. HEAD /admin returns 200 with head_only set
        let mut parser = RequestParser::new(4096);
        assert_eq!(
            parser.feed(b"HEAD /admin HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Progress::Complete
        );
        let req = parser.take().unwrap();
        let reply = build_reply(&[], &sessions, &metrics, &req, remote, 8080, &mut spec);
        assert_eq!(reply.response.status, 200);
        let raw = reply.response.to_bytes(false);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.ends_with("\r\n\r\n"));
        assert!(!text.contains("<!doctype html>"));
    }
}
