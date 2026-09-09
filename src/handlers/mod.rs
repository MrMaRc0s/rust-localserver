//! Request handlers: static files, uploads, deletion, directory listing and
//! redirects.
//!
//! Large files are streamed through the connection's write buffer in bounded
//! slices, one slice per writable event, rather than loaded whole — that keeps
//! a big download from starving other clients and keeps memory flat under
//! `siege`.
//!
//! Filled in by TASKS.md 5.1, 5.2, 5.3, 5.4. Audit [A-17]..[A-21], [A-26].
