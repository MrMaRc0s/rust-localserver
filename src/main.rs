//! LocalServer — a single-threaded, non-blocking HTTP/1.1 server.
//!
//! Architecture in one paragraph: one process, one thread, one `mio::Poll`
//! (an `epoll` instance). Listening sockets, client connections and CGI pipes
//! are all registered in that single `Poll`; the event loop in [`server`] does
//! exactly one `read` or one `write` syscall per ready token per iteration and
//! never blocks. See `TASKS.md` for the phased implementation plan and
//! `AUDIT_NOTES.md` for the line-referenced walkthrough.
//! never blocks. See `docs/TASKS.md` for the phased implementation plan and
//! `docs/AUDIT_NOTES.md` for the line-referenced walkthrough.

mod cgi;
mod config;
mod connection;
mod error;
mod handlers;
mod http;
mod router;
mod server;
mod utils;
mod metrics;

use std::path::Path;
use std::process::ExitCode;

use config::Config;

/// Config path used when none is given on the command line.
const DEFAULT_CONFIG: &str = "config.yaml";

/// Exit code for a configuration that cannot be used. Distinct from 1 so a test
/// script can tell "bad config" from any other failure.
const EXIT_CONFIG: u8 = 2;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let config_path = match args.next() {
        Some(arg) if arg == "-h" || arg == "--help" => {
            println!("Usage: localserver [CONFIG]\n\nCONFIG defaults to {DEFAULT_CONFIG}");
            return ExitCode::SUCCESS;
        }
        Some(path) => path,
        None => DEFAULT_CONFIG.to_string(),
    };

    // Validate before binding anything: a configuration error must surface at
    // startup, not when the first request arrives.
    let (config, issues) = match Config::load(Path::new(&config_path)) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("localserver: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    // Non-fatal issues: these servers were dropped, the rest still run.
    for issue in &issues {
        if issue.line > 0 {
            eprintln!("localserver: warning: {config_path}:{}: {}", issue.line, issue.message);
        } else {
            eprintln!("localserver: warning: {}", issue.message);
        }
    }
    if !issues.is_empty() {
        eprintln!(
            "localserver: {} server block(s) skipped, {} still configured",
            issues.len(),
            config.servers.len()
        );
    }

    print_summary(&config);

    // Bind first: a port already in use must be reported now, not silently.
    let mut server = match server::Server::bind(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("localserver: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    // `run` returns only if the poll itself fails, which is not recoverable:
    // without readiness notifications the process cannot serve anyone. Every
    // per-client failure is handled inside the loop and never reaches here.
    if let Err(e) = server.run() {
        eprintln!("localserver: event loop stopped: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Print what was actually loaded, so a config change can be confirmed without
/// sending a request. Useful during the audit's configuration questions.
fn print_summary(config: &Config) {
    println!("Loaded {} server block(s):", config.servers.len());
    for (host, port) in config.listeners() {
        let servers = config.servers_on(&host, port);
        println!("  listener {host}:{port} — {} server(s)", servers.len());
        for server in servers {
            let names = if server.server_names.is_empty() {
                "(no server_names)".to_string()
            } else {
                server.server_names.join(", ")
            };
            let flag = if server.is_default { " [default]" } else { "" };
            println!(
                "    {names}{flag}  body limit {} bytes, {} route(s)",
                server.client_max_body_size,
                server.routes.len()
            );
            for route in &server.routes {
                let methods: Vec<&str> = route.methods.iter().map(|m| m.as_str()).collect();
                let mut detail = format!("{} -> {}", route.path, route.root.display());
                if let Some(r) = &route.redirect {
                    detail = format!("{} -> {} {}", route.path, r.status, r.target);
                }
                if !route.cgi.is_empty() {
                    let exts: Vec<&str> = route.cgi.keys().map(|s| s.as_str()).collect();
                    detail.push_str(&format!(" (cgi: {})", exts.join(", ")));
                }
                if route.autoindex {
                    detail.push_str(" (autoindex)");
                }
                println!("      [{}] {detail}", methods.join(","));
            }
        }
    }
}
