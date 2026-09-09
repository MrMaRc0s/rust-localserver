//! CGI/1.1 execution.
//!
//! `std::process::Command` with piped stdio; the child's stdin and stdout are
//! set `O_NONBLOCK` via `libc::fcntl`, wrapped in `mio::unix::SourceFd` and
//! registered in the **same** `Poll` as every socket. The request body is
//! pumped into stdin one write per writable event and stdout collected one read
//! per readable event; children are reaped with `waitpid(WNOHANG)` on the loop
//! tick, and a runaway script is killed on timeout.
//!
//! This is the crux of audit question [A-8] — CGI pipes are reads and writes, so
//! a blocking `wait_with_output()` would both fail that question and stall every
//! other client for the script's entire runtime.
//!
//! Environment (TASKS.md 7.2) includes `PATH_INFO` as the subject requires, and
//! the child's working directory is the script's own directory so relative
//! `open("data.txt")` calls resolve correctly.
//!
//! Filled in by TASKS.md 7.1, 7.2, 7.3. Audit [A-28], [A-39].
