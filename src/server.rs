//! The event loop: the single `mio::Poll`, the listener set and the connection
//! registry.
//!
//! One `Poll`, one `Events` buffer, one `HashMap<Token, Connection>`. Tokens are
//! partitioned into ranges (listeners / connections / CGI pipes) so dispatch is
//! a range check. `poll()` is called with a timeout so the loop still wakes to
//! sweep timeouts and reap CGI children when no traffic arrives.
//!
//! One `epoll` instance rather than several: with two, a socket could become
//! ready in one while the thread sits blocked in the other's `wait()`, forcing
//! either a spin loop or a second thread — both ruled out by the subject.
//!
//! Filled in by TASKS.md 2.1, 2.2, 2.3, 2.6, 2.7. Audit: [A-2]..[A-4], [A-8].
