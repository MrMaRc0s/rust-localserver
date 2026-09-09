//! Status codes to error pages.
//!
//! Resolves a status against the server's configured `error_pages`, falling
//! back to an HTML string compiled into the binary when the configured file is
//! missing or unreadable. That fallback is load-bearing: an error page that
//! itself fails to load would otherwise recurse, and "never crash" is a hard
//! requirement.
//!
//! Covers at minimum 400, 403, 404, 405, 408, 413, 500, 501, 505.
//!
//! Filled in by TASKS.md 5.5. Audit [A-12], [A-25].
