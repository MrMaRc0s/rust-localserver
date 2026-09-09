//! File extension to `Content-Type` mapping.
//!
//! A static table rather than a crate: the set of types a static site needs is
//! small, and a wrong `Content-Type` is one of the first things visible in the
//! browser's network panel.
//!
//! Filled in by TASKS.md 3.4. Audit [A-24].
