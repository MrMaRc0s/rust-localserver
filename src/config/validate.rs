//! Startup validation of a parsed [`crate::config::Config`].
//!
//! Two severities, and the distinction is the answer to audit questions
//! [A-30] and [A-31]:
//!   * **Fatal** — unresolvable, kills startup with a non-zero exit. Chiefly a
//!     duplicate listener: the same `host:port` claimed twice by the same
//!     `server_name`, or two `is_default` servers on one `host:port`.
//!   * **Skip** — a single malformed `ServerConfig` (missing root, bad error
//!     page, unusable CGI interpreter) is logged and dropped; the remaining
//!     servers keep running. Startup aborts only if *zero* servers survive.
//!
//! Note that the same `host:port` with *distinct* `server_names` is legal
//! virtual hosting, not a conflict.
//!
//! Filled in by TASKS.md 1.3, unit-tested by TASKS.md 1.4.
