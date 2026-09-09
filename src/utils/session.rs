//! In-memory session store: `HashMap<SessionId, Session>` with expiry.
//!
//! Session ids come from `/dev/urandom` (32 bytes, hex-encoded) so they are not
//! guessable. Expired entries are swept on the event-loop tick. No locking is
//! needed because there is exactly one thread — which is the honest answer to
//! the subject's "use thread-safe data structures" tip.
//!
//! Filled in by TASKS.md 6.2. Audit [A-22].
