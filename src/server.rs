//! The event loop: one `Poll`, the listener set, and the connection registry.
//!
//! # One epoll, and why ([A-2], [A-3], [A-4])
//!
//! There is exactly one [`Poll`] (an `epoll` instance on Linux), one [`Events`]
//! buffer and one connection map in this struct. Listening sockets, client
//! sockets and — from Phase 7 — CGI pipes are all registered in it.
//!
//! `epoll` rather than `select`: `select` is O(number of watched fds) per call,
//! rebuilds its fd sets on every call, and cannot watch a descriptor numbered
//! above `FD_SETSIZE` (1024), which a server under `siege` reaches easily.
//! `epoll` keeps the interest list in the kernel and returns only the ready
//! descriptors.
//!
//! *One* instance rather than several because with two, a descriptor could
//! become ready in the first while the thread is blocked in the second's
//! `wait()`. Nothing would wake it, so the only ways out are spinning with a
//! zero timeout or adding a thread — and the subject allows neither. A single
//! instance makes readiness for every descriptor arrive at one place.
//!
//! # Blocking ([A-8])
//!
//! Nothing in this program blocks. `poll()` is the only call that ever waits,
//! and it waits with a timeout so the loop keeps ticking with no traffic — which
//! is what makes timeout sweeping possible at all. Every socket is non-blocking,
//! and no read or write happens except in response to a readiness event.
//!
//! # Never crash ([A-20])
//!
//! The loop treats no per-client problem as fatal. A failed `accept`, a broken
//! client socket, even a panic inside connection handling (caught by
//! `catch_unwind`) costs at most that one client.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::io;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use mio::event::Event;
use mio::net::{TcpListener, TcpStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};

use crate::config::{Config, ServerConfig};
use crate::cgi::{CgiProcess, CgiStep};
use crate::connection::{Action, Connection, Timeout};
use crate::error;
use crate::handlers::Reply;
use crate::metrics::Metrics;
use crate::utils::session::{SessionStore, DEFAULT_TTL};

/// Listener tokens occupy `[0, CONNECTION_TOKEN_BASE)`, connection tokens start
/// at `CONNECTION_TOKEN_BASE`. Dispatch is then a single comparison, and a
/// stale event for a closed connection cannot be mistaken for a listener.
const CONNECTION_TOKEN_BASE: usize = 1 << 20;

/// CGI pipe tokens start above every connection token, so the same single
/// comparison chain that separates listeners from connections also separates
/// connections from pipes. Three descriptors are registered per script — stdin,
/// stdout and stderr — in the **same** `Poll` as every socket, which is what
/// makes a script's I/O just another source of readiness rather than a reason
/// to block ([A-8]).
const CGI_TOKEN_BASE: usize = 2 << 20;

/// How long `poll()` may wait when nothing is ready. This is the resolution of
/// timeout sweeping, not a delay imposed on requests: any actual readiness wakes
/// the loop immediately.
const TICK: Duration = Duration::from_millis(500);

/// Events collected per `poll()`.
const EVENTS_CAPACITY: usize = 1024;

/// How often expired sessions are swept.
///
/// Not every tick: sweeping walks the whole map, and doing that twice a second
/// would make idle servers busy for no reason. Sessions expiring a minute late
/// costs nothing — the lookup checks expiry itself, so a stale entry is never
/// handed out, it just occupies memory a little longer.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Ceiling on simultaneous client connections.
///
/// Without one, a flood exhausts the process's file descriptors, `accept` starts
/// failing with `EMFILE`, and because the listener stays readable the loop spins
/// at 100% CPU retrying it. At the ceiling the listeners are deregistered
/// instead: pending connections wait in the kernel backlog, which is exactly
/// what a backlog is for.
const MAX_CONNECTIONS: usize = 512;

/// Which of a script's three pipes an event concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipe {
    Stdin,
    Stdout,
    Stderr,
}

/// A bound listening socket and the address it answers on.
struct Listener {
    socket: TcpListener,
    host: String,
    port: u16,
    token: Token,
    /// Servers sharing this address, in declaration order. Shared with every
    /// connection accepted here so the router can pick one by `Host`.
    ///
    /// `Rc` rather than `Arc`: there is exactly one thread, so atomic
    /// refcounting would be paid for and never used.
    servers: Rc<Vec<ServerConfig>>,
    /// The largest body any server sharing this listener accepts.
    ///
    /// Which virtual host a request belongs to is not known until its `Host`
    /// header arrives, so the limit applied while reading is the most
    /// permissive one on this address; the selected server's own stricter limit
    /// is applied as soon as the headers are parsed (TASKS.md 4.1).
    max_body: usize,
}

pub struct Server {
    poll: Poll,
    listeners: Vec<Listener>,
    connections: HashMap<Token, Connection>,
    /// Reuse closed connections' token numbers so the counter cannot run away
    /// during a long stress run.
    free_tokens: Vec<usize>,
    next_token: usize,
    /// True while listeners are deregistered because of [`MAX_CONNECTIONS`].
    throttled: bool,
    #[allow(dead_code)] // routing reads this from Phase 4 onward.
    config: Config,
    accepted: u64,
    /// Shared with every connection. One thread means `Rc<RefCell<_>>` is
    /// enough; see [`crate::utils::session`].
    sessions: Rc<RefCell<SessionStore>>,
    last_session_sweep: Instant,
    metrics: Rc<RefCell<Metrics>>,
    /// Running scripts, keyed by the connection waiting for each.
    cgi: HashMap<Token, CgiProcess>,
    /// Pipe token to the connection that owns it, so a pipe event can be routed
    /// back to its script in one lookup.
    cgi_pipes: HashMap<Token, Token>,
    next_cgi_token: usize,
    free_cgi_tokens: Vec<usize>,
}

impl Server {
    /// Bind every listener the configuration asks for.
    ///
    /// A listener that cannot be bound (port already in use, for instance) is
    /// reported and skipped rather than aborting startup — the same reasoning as
    /// the Skip severity in config validation ([A-31]): one unusable listener
    /// should not deny service on the others. Startup fails only if none bound.
    pub fn bind(config: Config) -> io::Result<Server> {
        // A browser closing a tab mid-response makes the kernel raise SIGPIPE on
        // the next write, whose default action terminates the process. Ignoring
        // it turns that into an ordinary EPIPE error return, which the
        // connection code already handles by dropping that one client. Without
        // this the server dies on a perfectly normal client behaviour ([A-20]).
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }

        let poll = Poll::new()?;
        let mut listeners = Vec::new();
        let mut failures = Vec::new();

        for (index, (host, port)) in config.listeners().into_iter().enumerate() {
            let ip = match config
                .servers_on(&host, port)
                .first()
                .and_then(|s| s.bind_addr())
            {
                Some(ip) => ip,
                None => {
                    failures.push(format!("{host}:{port}: no bindable address"));
                    continue;
                }
            };
            let addr = SocketAddr::new(ip, port);
            let mut socket = match TcpListener::bind(addr) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!("{host}:{port}: {e}"));
                    continue;
                }
            };
            let token = Token(index);
            if let Err(e) = poll
                .registry()
                .register(&mut socket, token, Interest::READABLE)
            {
                failures.push(format!("{host}:{port}: register: {e}"));
                continue;
            }
            let servers: Vec<ServerConfig> =
                config.servers_on(&host, port).into_iter().cloned().collect();
            let max_body = servers
                .iter()
                .map(|s| s.client_max_body_size)
                .max()
                .unwrap_or(crate::config::DEFAULT_MAX_BODY);
            listeners.push(Listener {
                socket,
                host,
                port,
                token,
                servers: Rc::new(servers),
                max_body,
            });
        }

        for failure in &failures {
            eprintln!("localserver: warning: could not listen on {failure}");
        }

        if listeners.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no listener could be bound",
            ));
        }

        for listener in &listeners {
            println!("localserver: listening on http://{}:{}", listener.host, listener.port);
        }

        Ok(Server {
            poll,
            listeners,
            connections: HashMap::new(),
            free_tokens: Vec::new(),
            next_token: CONNECTION_TOKEN_BASE,
            throttled: false,
            config,
            accepted: 0,
            sessions: Rc::new(RefCell::new(SessionStore::new(DEFAULT_TTL))),
            last_session_sweep: Instant::now(),
            metrics: Rc::new(RefCell::new(Metrics::new())),
            cgi: HashMap::new(),
            cgi_pipes: HashMap::new(),
            next_cgi_token: CGI_TOKEN_BASE,
            free_cgi_tokens: Vec::new(),
        })
    }

    /// The event loop. Returns only on an unrecoverable poll failure.
    pub fn run(&mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(EVENTS_CAPACITY);
        loop {
            // The single wait in the whole program.
            match self.poll.poll(&mut events, Some(TICK)) {
                Ok(()) => {}
                // A signal interrupted the wait. Not an error: poll again.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }

            for event in events.iter() {
                let token = event.token();
                if token.0 < CONNECTION_TOKEN_BASE {
                    self.accept_from(token);
                } else if token.0 < CGI_TOKEN_BASE {
                    self.dispatch(token, event);
                } else {
                    self.dispatch_cgi(token, event);
                }
            }

            // Runs on every iteration, including the ones where `poll` returned
            // because of the tick rather than because of traffic.
            self.sweep_timeouts();
            self.sweep_cgi();
            self.sweep_sessions();
        }
    }

    /// Accept every pending connection on a ready listener.
    ///
    /// This is the one place a drain loop is correct: each `accept()` returns a
    /// *different* descriptor, so looping here serves many clients rather than
    /// favouring one. It is not the "one read per client" rule ([A-5]) — that
    /// rule is about a single client's socket.
    fn accept_from(&mut self, token: Token) {
        let index = match self.listeners.iter().position(|l| l.token == token) {
            Some(i) => i,
            None => return,
        };

        loop {
            if self.connections.len() >= MAX_CONNECTIONS {
                self.throttle();
                return;
            }

            let max_body = self.listeners[index].max_body;
            let servers = Rc::clone(&self.listeners[index].servers);
            let sessions = Rc::clone(&self.sessions);
            let port = self.listeners[index].port;
            let (socket, peer) = match self.listeners[index].socket.accept() {
                Ok(pair) => pair,
                // Backlog drained: the normal way out of this loop.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // EMFILE, ENFILE, ECONNABORTED... none of these justify
                    // taking the server down; the existing clients are fine.
                    eprintln!(
                        "localserver: accept on {}:{} failed: {e}",
                        self.listeners[index].host, self.listeners[index].port
                    );
                    return;
                }
            };

            self.metrics.borrow_mut().record_connection_open();
            self.register_connection(socket, peer, max_body, port, servers, sessions);
        }
    }

    fn register_connection(
        &mut self,
        mut socket: TcpStream,
        peer: SocketAddr,
        max_body: usize,
        port: u16,
        servers: Rc<Vec<ServerConfig>>,
        sessions: Rc<RefCell<SessionStore>>,
    ) {
        // Small responses should leave immediately rather than waiting for
        // Nagle to coalesce them with data that will never come.
        if let Err(e) = socket.set_nodelay(true) {
            eprintln!("localserver: set_nodelay for {peer} failed: {e}");
        }

        let token = self.alloc_token();
        if let Err(e) = self
            .poll
            .registry()
            .register(&mut socket, token, Interest::READABLE)
        {
            // Could not watch it, so it can never be served: drop it now rather
            // than leak the descriptor.
            eprintln!("localserver: register {peer} failed: {e}");
            self.free_tokens.push(token.0);
            self.metrics.borrow_mut().record_connection_close();
            return;
        }

        let metrics = Rc::clone(&self.metrics);
        self.connections
            .insert(
                token,
                Connection::new(socket, peer, token, max_body, port, servers, sessions, metrics),
            );
        self.accepted += 1;
    }

    /// Handle one readiness event for one client.
    ///
    /// At most one read and at most one write happen here — see
    /// [`crate::connection`] for why. The `catch_unwind` is a last-resort net:
    /// a bug in request handling costs that one client, not the process
    /// ([A-20]).
    fn dispatch(&mut self, token: Token, event: &Event) {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let connection = match self.connections.get_mut(&token) {
                Some(c) => c,
                // An event for a connection closed earlier in this same batch.
                None => return None,
            };

            let mut action = Action::Keep;
            if event.is_readable() {
                action = connection.on_readable();
            } else if event.is_writable() {
                action = connection.on_writable();
            }
            Some((action, connection.desired_interest(), connection.take_rearm()))
        }));

        let (action, interest, rearm) = match outcome {
            Ok(Some(triple)) => triple,
            Ok(None) => return,
            Err(_) => {
                eprintln!("localserver: panic while handling {token:?}, dropping the client");
                self.close(token);
                return;
            }
        };

        match action {
            Action::Close => self.close(token),
            Action::Keep => self.sync_interest(token, interest, rearm),
            Action::StartCgi => {
                self.sync_interest(token, interest, rearm);
                self.start_cgi(token);
            }
        }
    }

    /// Re-register when the connection now waits for something different, or
    /// when it must be re-armed.
    ///
    /// `mio` registers descriptors edge-triggered, so a socket that still holds
    /// data after a single read will not be reported again on its own.
    /// `EPOLL_CTL_MOD` re-reports readiness on an edge-triggered descriptor,
    /// which is what lets this server keep to one read per event instead of
    /// draining each client until it blocks ([A-5]).
    fn sync_interest(&mut self, token: Token, interest: Interest, rearm: bool) {
        let connection = match self.connections.get_mut(&token) {
            Some(c) => c,
            None => return,
        };
        if connection.registered == interest && !rearm {
            return;
        }
        match self
            .poll
            .registry()
            .reregister(&mut connection.socket, token, interest)
        {
            Ok(()) => connection.registered = interest,
            Err(e) => {
                eprintln!("localserver: reregister {} failed: {e}", connection.peer);
                self.close(token);
            }
        }
    }

    /// Remove a client: deregister, shut the socket down, drop it.
    ///
    /// Dropping the [`Connection`] closes the file descriptor — that is the
    /// whole file-descriptor-leak defence ([A-33]), and why nothing here calls
    /// `close()` explicitly. Deregistering first stops the poll from reporting
    /// events for a descriptor that is about to disappear.
    fn close(&mut self, token: Token) {
        // A client that gives up mid-script must not leave the script running:
        // dropping the process kills and reaps it.
        if let Some(mut process) = self.cgi.remove(&token) {
            self.release_cgi_pipes(&mut process);
        }
        if let Some(mut connection) = self.connections.remove(&token) {
            let _ = self.poll.registry().deregister(&mut connection.socket);
            // Best effort: the peer may already be gone, which is not an error
            // worth reporting.
            let _ = connection.socket.shutdown(std::net::Shutdown::Both);
            self.free_tokens.push(token.0);
            self.metrics.borrow_mut().record_connection_close();
        }
        self.unthrottle();
    }

    fn alloc_token(&mut self) -> Token {
        match self.free_tokens.pop() {
            Some(number) => Token(number),
            None => {
                let number = self.next_token;
                self.next_token += 1;
                Token(number)
            }
        }
    }

    /// Stop accepting while at capacity, by taking the listeners out of the
    /// poll. New connections queue in the kernel backlog instead of spinning
    /// the loop on an `accept` that cannot succeed.
    fn throttle(&mut self) {
        if self.throttled {
            return;
        }
        for listener in &mut self.listeners {
            let _ = self.poll.registry().deregister(&mut listener.socket);
        }
        self.throttled = true;
        eprintln!(
            "localserver: {MAX_CONNECTIONS} connections in use, pausing accept until one closes"
        );
    }

    fn unthrottle(&mut self) {
        if !self.throttled || self.connections.len() >= MAX_CONNECTIONS {
            return;
        }
        for listener in &mut self.listeners {
            if let Err(e) = self.poll.registry().register(
                &mut listener.socket,
                listener.token,
                Interest::READABLE,
            ) {
                eprintln!(
                    "localserver: could not resume accept on {}:{}: {e}",
                    listener.host, listener.port
                );
                return;
            }
        }
        self.throttled = false;
        eprintln!("localserver: resuming accept");
    }

    // ---------------------------------------------------------------------
    // CGI
    // ---------------------------------------------------------------------

    /// Spawn the script a connection asked for and register its pipes.
    ///
    /// This is the heart of audit question [A-8]: the three pipes go into the
    /// **same** `Poll` as every socket, so the script's I/O is just more
    /// readiness to react to. The alternative — `wait_with_output()` — would
    /// block the single thread for the script's entire runtime and freeze every
    /// other client.
    fn start_cgi(&mut self, connection: Token) {
        let spec = match self.connections.get_mut(&connection).and_then(|c| c.take_cgi_spec()) {
            Some(spec) => spec,
            None => return,
        };

        let stdin_token = Token(self.alloc_cgi_token());
        let stdout_token = Token(self.alloc_cgi_token());
        let stderr_token = Token(self.alloc_cgi_token());

        let mut process = match CgiProcess::spawn(&spec, stdin_token, stdout_token, stderr_token) {
            Ok(process) => {
                self.metrics.borrow_mut().record_cgi_spawn();
                process
            }
            Err(e) => {
                eprintln!(
                    "localserver: could not start {}: {e}",
                    spec.interpreter.display()
                );
                self.free_cgi_tokens.extend([stdin_token.0, stdout_token.0, stderr_token.0]);
                self.finish_cgi_with_status(connection, 502);
                return;
            }
        };

        // stdout and stderr are always watched; stdin only while there is a
        // body to hand over, and it is closed immediately otherwise so a script
        // reading to EOF is not left waiting.
        let mut registered: Vec<Token> = Vec::new();
        if process.has_input() {
            if let Some(fd) = process.stdin_fd() {
                if self
                    .poll
                    .registry()
                    .register(&mut SourceFd(&fd), stdin_token, Interest::WRITABLE)
                    .is_ok()
                {
                    registered.push(stdin_token);
                } else {
                    process.close_stdin();
                }
            }
        } else {
            process.close_stdin();
        }

        for (fd, token) in [
            (process.stdout_fd(), stdout_token),
            (process.stderr_fd(), stderr_token),
        ] {
            if let Some(fd) = fd {
                if self
                    .poll
                    .registry()
                    .register(&mut SourceFd(&fd), token, Interest::READABLE)
                    .is_ok()
                {
                    registered.push(token);
                }
            }
        }

        if registered.is_empty() {
            eprintln!("localserver: no CGI pipe could be registered, answering 502");
            self.finish_cgi_with_status(connection, 502);
            return;
        }

        for token in registered {
            self.cgi_pipes.insert(token, connection);
        }
        self.cgi.insert(connection, process);
    }

    /// One readiness event on one CGI pipe: one read, or one write.
    /// Wrapped in `catch_unwind` like [`Self::dispatch`]: a bug in CGI
    /// handling must drop that one client (502), not the whole process.
    fn dispatch_cgi(&mut self, token: Token, event: &Event) {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.dispatch_cgi_inner(token, event)
        }));
        if outcome.is_err() {
            eprintln!("localserver: panic while handling CGI pipe {token:?}, dropping the client");
            // Find the owning connection to close it cleanly.
            if let Some(connection) = self.cgi_pipes.get(&token).copied() {
                self.kill_cgi(connection);
                self.finish_cgi_with_status(connection, 502);
                self.close(connection);
            }
        }
    }

    fn dispatch_cgi_inner(&mut self, token: Token, event: &Event) {
        let connection = match self.cgi_pipes.get(&token) {
            Some(connection) => *connection,
            // A pipe event for a script that has already been torn down.
            None => return,
        };

        let (step, which) = {
            let process = match self.cgi.get_mut(&connection) {
                Some(process) => process,
                None => return,
            };
            if token == process.stdin_token && event.is_writable() {
                (process.on_stdin_writable(), Pipe::Stdin)
            } else if token == process.stdout_token {
                (process.on_stdout_readable(), Pipe::Stdout)
            } else if token == process.stderr_token {
                (process.on_stderr_readable(), Pipe::Stderr)
            } else {
                return;
            }
        };

        match step {
            // More to move on this pipe. The registration is edge-triggered
            // like every other, so a pipe that is still ready after one
            // syscall reports nothing further on its own: a body larger than
            // the 64 KB pipe buffer, or output larger than one read, would
            // stall halfway. Re-arming reports readiness again if there is any,
            // and stays silent if there is not — no spin either way.
            CgiStep::Continue => self.rearm_cgi_pipe(connection, token, which),
            CgiStep::Closed => self.close_cgi_pipe(connection, token, which),
            CgiStep::Failed(status) => {
                // Output cap exceeded, or an unusable pipe.
                self.kill_cgi(connection);
                self.finish_cgi_with_status(connection, status);
                return;
            }
        }

        // Both output streams at EOF: the script has said all it will.
        let complete = self
            .cgi
            .get(&connection)
            .map(|process| process.output_complete())
            .unwrap_or(false);
        if complete {
            self.finish_cgi(connection);
        }
    }

    /// Re-arm one pipe's edge-triggered registration.
    fn rearm_cgi_pipe(&mut self, connection: Token, token: Token, which: Pipe) {
        let process = match self.cgi.get(&connection) {
            Some(process) => process,
            None => return,
        };
        let (fd, interest) = match which {
            Pipe::Stdin => (process.stdin_fd(), Interest::WRITABLE),
            Pipe::Stdout => (process.stdout_fd(), Interest::READABLE),
            Pipe::Stderr => (process.stderr_fd(), Interest::READABLE),
        };
        if let Some(fd) = fd {
            let _ = self
                .poll
                .registry()
                .reregister(&mut SourceFd(&fd), token, interest);
        }
    }

    /// Deregister one pipe and close it.
    fn close_cgi_pipe(&mut self, connection: Token, token: Token, which: Pipe) {
        if let Some(process) = self.cgi.get_mut(&connection) {
            // Deregister before dropping: the descriptor must still be open for
            // the poll to stop watching it.
            let fd = match which {
                Pipe::Stdin => process.stdin_fd(),
                Pipe::Stdout => process.stdout_fd(),
                Pipe::Stderr => process.stderr_fd(),
            };
            if let Some(fd) = fd {
                let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
            }
            match which {
                Pipe::Stdin => process.close_stdin(),
                Pipe::Stdout => process.close_stdout(),
                Pipe::Stderr => process.close_stderr(),
            }
        }
        self.cgi_pipes.remove(&token);
        self.free_cgi_tokens.push(token.0);
    }

    /// Take the script's output, turn it into a response, and wake the client.
    fn finish_cgi(&mut self, connection: Token) {
        let mut process = match self.cgi.remove(&connection) {
            Some(process) => process,
            None => return,
        };
        self.release_cgi_pipes(&mut process);

        // waitpid(WNOHANG): report the exit without waiting for it, so a script
        // that has finished does not linger as a zombie.
        let code = process.reap();
        let stderr = process.stderr_text();
        if !stderr.is_empty() {
            eprintln!("localserver: CGI stderr: {stderr}");
        }
        if let Some(code) = code {
            if code != 0 {
                eprintln!("localserver: CGI exited with status {code}");
            }
        }

        let response = process.into_response();
        self.wake_connection(connection, Reply::immediate(response));
    }

    /// Answer the waiting client with an error instead of script output.
    fn finish_cgi_with_status(&mut self, connection: Token, status: u16) {
        self.metrics.borrow_mut().record_cgi_failure();
        if let Some(mut process) = self.cgi.remove(&connection) {
            self.release_cgi_pipes(&mut process);
        }
        let response = error::builtin(status);
        self.wake_connection(connection, Reply::immediate(response));
    }

    /// Deregister and forget whatever pipes a process still holds.
    fn release_cgi_pipes(&mut self, process: &mut CgiProcess) {
        for (fd, token) in [
            (process.stdin_fd(), process.stdin_token),
            (process.stdout_fd(), process.stdout_token),
            (process.stderr_fd(), process.stderr_token),
        ] {
            if let Some(fd) = fd {
                let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
            }
            if self.cgi_pipes.remove(&token).is_some() {
                self.free_cgi_tokens.push(token.0);
            }
        }
        process.close_stdin();
        process.close_stdout();
        process.close_stderr();
    }

    fn kill_cgi(&mut self, connection: Token) {
        self.metrics.borrow_mut().record_cgi_timeout();
        if let Some(process) = self.cgi.get_mut(&connection) {
            process.kill();
        }
    }

    /// Hand a reply to the connection that was parked waiting for it.
    fn wake_connection(&mut self, token: Token, reply: Reply) {
        let action = match self.connections.get_mut(&token) {
            Some(connection) => connection.deliver_cgi(reply),
            // The client gave up while the script ran.
            None => return,
        };
        match action {
            Action::Close => self.close(token),
            Action::StartCgi | Action::Keep => {
                let interest = match self.connections.get(&token) {
                    Some(connection) => connection.desired_interest(),
                    None => return,
                };
                // Force the re-register: the connection was already readable,
                // and without re-arming an edge-triggered registration would
                // never report it writable.
                self.sync_interest(token, interest, true);
            }
        }
    }

    /// Kill scripts that overran their deadline, and reap any that exited
    /// without their pipes reaching EOF.
    ///
    /// Without this an infinite loop in a script is a permanent process and
    /// three permanent file descriptors ([A-33], [A-34]).
    fn sweep_cgi(&mut self) {
        if self.cgi.is_empty() {
            return;
        }
        let now = Instant::now();
        let expired: Vec<Token> = self
            .cgi
            .iter()
            .filter(|(_, process)| process.timed_out(now))
            .map(|(connection, _)| *connection)
            .collect();

        for connection in expired {
            eprintln!("localserver: CGI script exceeded its deadline, killing it");
            self.kill_cgi(connection);
            self.finish_cgi_with_status(connection, 504);
        }
    }

    fn alloc_cgi_token(&mut self) -> usize {
        match self.free_cgi_tokens.pop() {
            Some(number) => number,
            None => {
                let number = self.next_cgi_token;
                self.next_cgi_token += 1;
                number
            }
        }
    }

    /// Drop expired sessions, at most once a minute.
    fn sweep_sessions(&mut self) {
        if self.last_session_sweep.elapsed() < SESSION_SWEEP_INTERVAL {
            return;
        }
        self.last_session_sweep = Instant::now();
        // A failed borrow would mean a connection is mid-request with the store
        // open; skipping this sweep is harmless, the next one will catch up.
        if let Ok(mut store) = self.sessions.try_borrow_mut() {
            let dropped = store.sweep();
            if dropped > 0 {
                eprintln!(
                    "localserver: swept {dropped} expired session(s), {} remain",
                    store.len()
                );
            }
        }
    }

    /// Enforce both deadlines on every connection, every iteration.
    ///
    /// Without this a client that opens a socket and sends `GET / HTTP/1.1\r\n`
    /// and then nothing holds a descriptor forever ([A-34]).
    fn sweep_timeouts(&mut self) {
        if self.connections.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut idle = Vec::new();
        let mut hard = Vec::new();

        for (token, connection) in &self.connections {
            match connection.timeout(now) {
                Timeout::None => {}
                Timeout::Idle => idle.push(*token),
                Timeout::Hard => hard.push(*token),
            }
        }

        // The total-duration deadline is absolute: no grace, no final response.
        for token in hard {
            if let Some(connection) = self.connections.get(&token) {
                eprintln!(
                    "localserver: {} exceeded the request deadline after {:?}, closing",
                    connection.peer,
                    connection.age()
                );
            }
            self.close(token);
        }

        // An idle connection may still get a 408 out before it goes.
        for token in idle {
            let action = match self.connections.get_mut(&token) {
                Some(connection) => connection.on_idle_timeout(),
                None => continue,
            };
            match action {
                Action::Close => self.close(token),
                // A timing-out connection never starts a script.
                Action::StartCgi | Action::Keep => {
                    let interest = match self.connections.get(&token) {
                        Some(c) => c.desired_interest(),
                        None => continue,
                    };
                    self.sync_interest(token, interest, false);
                }
            }
        }
    }
}
