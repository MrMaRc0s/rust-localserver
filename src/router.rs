//! Resolution of `(Host header, request path, method)` to a concrete action.
//!
//! Two stages. First virtual-host selection: match `Host` (minus any `:port`,
//! lowercased) against `server_names`, falling back to the `is_default` server
//! for that listener — this is what lets two sites share `127.0.0.1:8080`
//! ([A-11]). Then longest-prefix route matching, and within a route the order
//! is deliberate: redirect wins over method checking (the resource is not being
//! served), and 405 precedes 404 (whether the resource exists is not the
//! question being answered).
//!
//! Filled in by TASKS.md 4.1, 4.2; unit-tested by TASKS.md 4.4.
//! Audit [A-14]..[A-16], [A-27], [A-38].
