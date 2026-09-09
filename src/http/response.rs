//! Response construction and serialization.
//!
//! Always emits `Date`, `Server`, and either `Content-Length` or
//! `Transfer-Encoding: chunked` — browsers hang on a missing or wrong
//! `Content-Length`, which reads at the audit as a broken server. Honours
//! `Connection: keep-alive` (the HTTP/1.1 default) and `close`.
//!
//! Filled in by TASKS.md 3.4. Audit [A-24].
