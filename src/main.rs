//! LocalServer — a single-threaded, non-blocking HTTP/1.1 server.
//!
//! Architecture in one paragraph: one process, one thread, one `mio::Poll`
//! (an `epoll` instance). Listening sockets, client connections and CGI pipes
//! are all registered in that single `Poll`; the event loop in [`server`] does
//! exactly one `read` or one `write` syscall per ready token per iteration and
//! never blocks. See `TASKS.md` for the phased implementation plan and
//! `AUDIT_NOTES.md` for the line-referenced walkthrough.

mod cgi;
mod config;
mod connection;
mod error;
mod handlers;
mod http;
mod router;
mod server;
mod utils;

use std::process::ExitCode;

/// Config path used when none is given on the command line.
const DEFAULT_CONFIG: &str = "config.yaml";

fn main() -> ExitCode {
    // Usage: localserver [path/to/config.yaml]
    let mut args = std::env::args().skip(1);
    let config_path = match args.next() {
        Some(arg) if arg == "-h" || arg == "--help" => {
            println!("Usage: localserver [CONFIG]\n\nCONFIG defaults to {DEFAULT_CONFIG}");
            return ExitCode::SUCCESS;
        }
        Some(path) => path,
        None => DEFAULT_CONFIG.to_string(),
    };

    // TASKS.md 1.2/1.3: load + validate the config, then TASKS.md 2.x: run the loop.
    // Startup failures exit non-zero with a message on stderr; once the loop is
    // running it must never terminate on a per-client error.
    eprintln!("localserver: skeleton build, config would be loaded from {config_path:?}");
    ExitCode::SUCCESS
}
