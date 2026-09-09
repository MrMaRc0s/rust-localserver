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

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use mio::event::Event;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

use crate::config::Config;
use crate::connection::{Action, Connection, Timeout};

/// Listener tokens occupy `[0, CONNECTION_TOKEN_BASE)`, connection tokens start
/// at `CONNECTION_TOKEN_BASE`. Dispatch is then a single comparison, and a
/// stale event for a closed connection cannot be mistaken for a listener.
const CONNECTION_TOKEN_BASE: usize = 1 << 20;

/// How long `poll()` may wait when nothing is ready. This is the resolution of
/// timeout sweeping, not a delay imposed on requests: any actual readiness wakes
/// the loop immediately.
const TICK: Duration = Duration::from_millis(500);

/// Events collected per `poll()`.
const EVENTS_CAPACITY: usize = 1024;

/// Ceiling on simultaneous client connections.
///
/// Without one, a flood exhausts the process's file descriptors, `accept` starts
/// failing with `EMFILE`, and because the listener stays readable the loop spins
/// at 100% CPU retrying it. At the ceiling the listeners are deregistered
/// instead: pending connections wait in the kernel backlog, which is exactly
/// what a backlog is for.
const MAX_CONNECTIONS: usize = 512;

/// A bound listening socket and the address it answers on.
struct Listener {
    socket: TcpListener,
    host: String,
    port: u16,
    token: Token,
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
            listeners.push(Listener { socket, host, port, token });
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
                } else {
                    self.dispatch(token, event);
                }
            }

            // Runs on every iteration, including the ones where `poll` returned
            // because of the tick rather than because of traffic.
            self.sweep_timeouts();
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

            self.register_connection(socket, peer);
        }
    }

    fn register_connection(&mut self, mut socket: TcpStream, peer: SocketAddr) {
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
            return;
        }

        self.connections
            .insert(token, Connection::new(socket, peer, token));
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
            }
            if action == Action::Keep && event.is_writable() {
                action = connection.on_writable();
            }
            Some((action, connection.desired_interest()))
        }));

        let (action, interest) = match outcome {
            Ok(Some(pair)) => pair,
            Ok(None) => return,
            Err(_) => {
                eprintln!("localserver: panic while handling {token:?}, dropping the client");
                self.close(token);
                return;
            }
        };

        match action {
            Action::Close => self.close(token),
            Action::Keep => self.sync_interest(token, interest),
        }
    }

    /// Re-register only when the connection now waits for something different.
    fn sync_interest(&mut self, token: Token, interest: Interest) {
        let connection = match self.connections.get_mut(&token) {
            Some(c) => c,
            None => return,
        };
        if connection.registered == interest {
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
        if let Some(mut connection) = self.connections.remove(&token) {
            let _ = self.poll.registry().deregister(&mut connection.socket);
            // Best effort: the peer may already be gone, which is not an error
            // worth reporting.
            let _ = connection.socket.shutdown(std::net::Shutdown::Both);
            self.free_tokens.push(token.0);
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
                Action::Keep => {
                    let interest = match self.connections.get(&token) {
                        Some(c) => c.desired_interest(),
                        None => continue,
                    };
                    self.sync_interest(token, interest);
                }
            }
        }
    }
}
