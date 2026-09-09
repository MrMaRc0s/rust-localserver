//! Per-client state machine and the only place client socket I/O happens.
//!
//! States: `ReadingHeaders -> ReadingBody -> Handling -> Writing -> (KeepAlive |
//! Closing)`. Each connection owns its read and write buffers so a partially
//! received request or a partially flushed response survives across event-loop
//! iterations.
//!
//! Invariant, and the point of audit question [A-5]: **exactly one `read()` or
//! one `write()` syscall per client per readiness event.** Draining a single
//! client until `WouldBlock` would let one peer monopolise the thread;
//! level-triggered registration re-reports the remainder on the next `poll()`.
//!
//! Error policy ([A-6], [A-7]): `Ok(0)` means the peer closed — drop the client.
//! `WouldBlock`/`Interrupted` are *not* errors and must not drop it. Any other
//! `Err` deregisters the socket and removes the connection, which closes the fd
//! via `Drop` and is the file-descriptor-leak defence.
//!
//! Filled in by TASKS.md 2.4, 2.5, 2.6.
