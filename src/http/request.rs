//! Incremental HTTP request parser.
//!
//! Must be restartable: because the loop performs one `read()` per event, bytes
//! arrive in arbitrary fragments. The parser therefore distinguishes
//! `Incomplete` (need more bytes, keep the buffer, wait for the next event)
//! from `Error` (malformed — respond 4xx and close), and never blocks waiting
//! for the rest of a message.
//!
//! Filled in by TASKS.md 3.1, 3.2, 3.3; unit-tested by TASKS.md 3.5. Audit [A-36].
