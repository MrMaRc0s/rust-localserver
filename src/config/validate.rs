//! Startup validation of a parsed [`Config`].
//!
//! # Two severities, and why
//!
//! * [`Severity::Fatal`] — the configuration cannot be honoured at all, so
//!   startup stops with a non-zero exit. Reserved for **listener conflicts**:
//!   the same `host:port` claimed twice by the same name, two default servers
//!   on one listener, or a port repeated inside one server's own `ports` list.
//!   These are unresolvable — the server cannot guess which block wins — and
//!   discovering them at `bind()` time in production is far worse than at
//!   startup. This is audit question [A-30].
//!
//! * [`Severity::Skip`] — one server block is broken (missing root, unreadable
//!   error page, absurd body limit). That block is dropped with a logged reason
//!   and the others keep serving. Startup fails only if *no* server survives.
//!   This is audit question [A-31]: an error in one server's configuration must
//!   not bring down the others.
//!
//! # Ordering
//!
//! Per-server checks run **first**, then conflict detection runs over the
//! survivors only. A server that has been dropped will never bind a socket, so
//! it cannot conflict with anything; checking it would turn one broken block
//! into a fatal error and defeat the whole point of the Skip severity.
//!
//! # Not a conflict
//!
//! The same `host:port` with *distinct* `server_names` is ordinary virtual
//! hosting: one listening socket, requests dispatched by `Host` header. Only an
//! ambiguous claim on the same name is an error.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{Config, Method, ServerConfig, MAX_BODY_CEILING};

/// Status codes a `redirect:` may use.
const REDIRECT_STATUSES: &[u16] = &[301, 302, 303, 307, 308];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Refuse to start.
    Fatal,
    /// Drop this server, keep the rest.
    Skip,
}

/// One validation finding. `line` is 0 when the issue is not tied to a
/// particular line (e.g. "no usable servers remain").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub line: usize,
    pub message: String,
}

impl Issue {
    fn fatal(line: usize, message: impl Into<String>) -> Issue {
        Issue { severity: Severity::Fatal, line, message: message.into() }
    }

    fn skip(line: usize, message: impl Into<String>) -> Issue {
        Issue { severity: Severity::Skip, line, message: message.into() }
    }
}

/// Servers that survived validation, plus the reasons any others were dropped.
#[derive(Debug, Clone)]
pub struct Validated {
    pub servers: Vec<ServerConfig>,
    pub issues: Vec<Issue>,
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_readable_file(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_writable_dir(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_dir() && !m.permissions().readonly())
        .unwrap_or(false)
}

/// Check one server block. Returns the reason it must be dropped, or `None`.
fn check_server(server: &ServerConfig) -> Option<Issue> {
    if server.ports.is_empty() {
        return Some(Issue::skip(server.line, "server declares no ports"));
    }
    if server.ports.contains(&0) {
        return Some(Issue::skip(server.line, "port 0 is not a usable listening port"));
    }
    if server.bind_addr().is_none() {
        return Some(Issue::skip(
            server.line,
            format!("`host: {}` is not an IP address the server can bind", server.host),
        ));
    }
    if server.client_max_body_size == 0 {
        return Some(Issue::skip(
            server.line,
            "`client_max_body_size` is 0, which would reject every request with a body",
        ));
    }
    if server.client_max_body_size > MAX_BODY_CEILING {
        return Some(Issue::skip(
            server.line,
            format!(
                "`client_max_body_size` of {} exceeds the {} ceiling",
                server.client_max_body_size, MAX_BODY_CEILING
            ),
        ));
    }
    if server.routes.is_empty() {
        return Some(Issue::skip(server.line, "server declares no routes"));
    }

    for (code, page) in &server.error_pages {
        if !is_readable_file(page) {
            return Some(Issue::skip(
                server.line,
                format!("error page for {code} is unreadable: {}", page.display()),
            ));
        }
    }

    let mut seen_paths: Vec<&str> = Vec::new();
    for route in &server.routes {
        if !route.path.starts_with('/') {
            return Some(Issue::skip(
                route.line,
                format!("route path `{}` must start with `/`", route.path),
            ));
        }
        if seen_paths.contains(&route.path.as_str()) {
            return Some(Issue::skip(
                route.line,
                format!("duplicate route path `{}`", route.path),
            ));
        }
        seen_paths.push(&route.path);

        if route.methods.is_empty() {
            return Some(Issue::skip(
                route.line,
                format!("route `{}` accepts no methods", route.path),
            ));
        }

        if let Some(redirect) = &route.redirect {
            if !REDIRECT_STATUSES.contains(&redirect.status) {
                return Some(Issue::skip(
                    route.line,
                    format!(
                        "route `{}`: {} is not a redirect status (expected one of {:?})",
                        route.path, redirect.status, REDIRECT_STATUSES
                    ),
                ));
            }
            if redirect.target.is_empty() {
                return Some(Issue::skip(
                    route.line,
                    format!("route `{}`: redirect target is empty", route.path),
                ));
            }
            // A redirect answers before any file lookup, so its root is never used.
            continue;
        }

        if !is_dir(&route.root) {
            return Some(Issue::skip(
                route.line,
                format!(
                    "route `{}`: root `{}` does not exist or is not a directory",
                    route.path,
                    route.root.display()
                ),
            ));
        }

        if let Some(index) = &route.index {
            if index.is_empty() || index.contains('/') {
                return Some(Issue::skip(
                    route.line,
                    format!("route `{}`: `index` must be a bare file name, got `{index}`", route.path),
                ));
            }
        }

        for (ext, interpreter) in &route.cgi {
            if !ext.starts_with('.') {
                return Some(Issue::skip(
                    route.line,
                    format!("route `{}`: CGI extension `{ext}` must start with `.`", route.path),
                ));
            }
            if !is_executable(interpreter) {
                return Some(Issue::skip(
                    route.line,
                    format!(
                        "route `{}`: CGI interpreter for `{ext}` is not executable: {}",
                        route.path,
                        interpreter.display()
                    ),
                ));
            }
        }

        if let Some(dir) = &route.upload_dir {
            if !is_dir(dir) {
                return Some(Issue::skip(
                    route.line,
                    format!(
                        "route `{}`: upload_dir `{}` does not exist or is not a directory",
                        route.path,
                        dir.display()
                    ),
                ));
            }
            if !is_writable_dir(dir) {
                return Some(Issue::skip(
                    route.line,
                    format!("route `{}`: upload_dir `{}` is not writable", route.path, dir.display()),
                ));
            }
        } else if route.methods.contains(&Method::Post) {
            // Not fatal: a POST route may exist purely to feed CGI.
        }
    }

    None
}

/// Detect listener conflicts among servers that are otherwise usable.
fn check_conflicts(servers: &[ServerConfig]) -> Vec<Issue> {
    let mut issues = Vec::new();

    // A single server listing the same port twice is the simplest form of
    // "the same port configured multiple times".
    for server in servers {
        let mut seen: Vec<u16> = Vec::new();
        for &port in &server.ports {
            if seen.contains(&port) {
                issues.push(Issue::fatal(
                    server.line,
                    format!("port {port} is listed twice in the same server"),
                ));
            } else {
                seen.push(port);
            }
        }
    }

    // Group by listener. Two servers may share one only if they can be told
    // apart by the Host header.
    let mut groups: BTreeMap<(String, u16), Vec<&ServerConfig>> = BTreeMap::new();
    for server in servers {
        for &port in &server.ports {
            groups.entry((server.host.clone(), port)).or_default().push(server);
        }
    }

    for ((host, port), group) in groups {
        if group.len() < 2 {
            continue;
        }

        let defaults: Vec<&&ServerConfig> = group.iter().filter(|s| s.is_default).collect();
        if defaults.len() > 1 {
            issues.push(Issue::fatal(
                defaults[1].line,
                format!(
                    "{host}:{port} has {} servers marked `is_default`; only one can answer \
                     requests whose Host matches no server_name",
                    defaults.len()
                ),
            ));
        }

        // Two catch-all servers (no server_names) on one listener are
        // indistinguishable, so neither can be selected reliably.
        let anonymous: Vec<&&ServerConfig> = group.iter().filter(|s| s.server_names.is_empty()).collect();
        if anonymous.len() > 1 {
            issues.push(Issue::fatal(
                anonymous[1].line,
                format!(
                    "{host}:{port} has {} servers with no `server_names`; they cannot be \
                     distinguished by Host header",
                    anonymous.len()
                ),
            ));
        }

        // The same virtual host name claimed twice on one listener.
        let mut claimed: Vec<(&str, usize)> = Vec::new();
        for server in &group {
            for name in &server.server_names {
                if let Some((_, first)) = claimed.iter().find(|(n, _)| *n == name.as_str()) {
                    issues.push(Issue::fatal(
                        server.line,
                        format!(
                            "server_name `{name}` is claimed twice on {host}:{port} \
                             (first at line {first})"
                        ),
                    ));
                } else {
                    claimed.push((name, server.line));
                }
            }
        }
    }

    issues
}

/// Validate a parsed config.
///
/// `Ok` carries the servers that may run plus the non-fatal issues explaining
/// any that were dropped. `Err` carries the fatal issues: startup must stop.
pub fn validate(config: Config) -> Result<Validated, Vec<Issue>> {
    let mut issues = Vec::new();
    let mut survivors = Vec::new();

    for server in config.servers {
        match check_server(&server) {
            Some(issue) => issues.push(issue),
            None => survivors.push(server),
        }
    }

    if survivors.is_empty() {
        // Every block was broken: there is nothing left to serve, so the Skip
        // issues become the reason for a fatal exit.
        let mut fatal: Vec<Issue> = issues
            .into_iter()
            .map(|i| Issue { severity: Severity::Fatal, ..i })
            .collect();
        fatal.push(Issue::fatal(0, "no usable server configuration remains"));
        return Err(fatal);
    }

    let conflicts = check_conflicts(&survivors);
    if !conflicts.is_empty() {
        return Err(conflicts);
    }

    Ok(Validated { servers: survivors, issues })
}

// ---------------------------------------------------------------------------
// Tests — audit questions [A-30] (duplicate port must be caught), [A-31] (one
// bad server must not stop the others) and [A-37] (configuration validation
// tests exist).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Redirect, RouteConfig};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A real directory that exists for the lifetime of one test, so root and
    /// upload_dir checks are exercised against the filesystem rather than
    /// mocked away.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let mut path = std::env::temp_dir();
            path.push(format!("localserver-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn route(path: &str, root: &Path) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            methods: vec![Method::Get],
            root: root.to_path_buf(),
            index: None,
            autoindex: false,
            redirect: None,
            cgi: BTreeMap::new(),
            upload_dir: None,
            line: 10,
        }
    }

    fn server(line: usize, names: &[&str], ports: &[u16], root: &Path) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            ports: ports.to_vec(),
            server_names: names.iter().map(|n| n.to_string()).collect(),
            is_default: false,
            client_max_body_size: 1024 * 1024,
            error_pages: BTreeMap::new(),
            routes: vec![route("/", root)],
            line,
        }
    }

    // --- Fatal: listener conflicts ([A-30]) ---------------------------------

    #[test]
    fn same_port_twice_in_one_server_is_fatal() {
        let dir = TempDir::new("dup-own-port");
        let config = Config { servers: vec![server(1, &["a.test"], &[8080, 8080], &dir.path)] };
        let fatal = validate(config).unwrap_err();
        assert!(fatal.iter().all(|i| i.severity == Severity::Fatal));
        assert!(
            fatal.iter().any(|i| i.message.contains("listed twice")),
            "got: {fatal:?}"
        );
    }

    #[test]
    fn same_server_name_on_one_listener_is_fatal() {
        let dir = TempDir::new("dup-name");
        let config = Config {
            servers: vec![
                server(1, &["shared.test"], &[8080], &dir.path),
                server(20, &["shared.test"], &[8080], &dir.path),
            ],
        };
        let fatal = validate(config).unwrap_err();
        assert!(
            fatal.iter().any(|i| i.message.contains("claimed twice")),
            "got: {fatal:?}"
        );
        // The error points at the second declaration, which is the one to delete.
        assert_eq!(fatal[0].line, 20);
    }

    #[test]
    fn two_default_servers_on_one_listener_is_fatal() {
        let dir = TempDir::new("dup-default");
        let mut a = server(1, &["a.test"], &[8080], &dir.path);
        let mut b = server(20, &["b.test"], &[8080], &dir.path);
        a.is_default = true;
        b.is_default = true;
        let fatal = validate(Config { servers: vec![a, b] }).unwrap_err();
        assert!(fatal.iter().any(|i| i.message.contains("is_default")), "got: {fatal:?}");
    }

    #[test]
    fn two_nameless_servers_on_one_listener_is_fatal() {
        let dir = TempDir::new("dup-anon");
        let config = Config {
            servers: vec![
                server(1, &[], &[8080], &dir.path),
                server(20, &[], &[8080], &dir.path),
            ],
        };
        let fatal = validate(config).unwrap_err();
        assert!(
            fatal.iter().any(|i| i.message.contains("no `server_names`")),
            "got: {fatal:?}"
        );
    }

    /// The counterpart of the tests above: sharing a port is legal as long as
    /// the Host header can tell the servers apart. Without this test, a
    /// too-eager conflict check would look correct.
    #[test]
    fn same_port_with_different_names_is_accepted() {
        let dir = TempDir::new("vhost");
        let config = Config {
            servers: vec![
                server(1, &["first.test"], &[8080], &dir.path),
                server(20, &["second.test"], &[8080], &dir.path),
            ],
        };
        let validated = validate(config).expect("virtual hosting must be allowed");
        assert_eq!(validated.servers.len(), 2);
        assert!(validated.issues.is_empty());
    }

    #[test]
    fn different_ports_never_conflict() {
        let dir = TempDir::new("ports");
        let config = Config {
            servers: vec![
                server(1, &["a.test"], &[8080], &dir.path),
                server(20, &["a.test"], &[8081], &dir.path),
            ],
        };
        assert_eq!(validate(config).unwrap().servers.len(), 2);
    }

    // --- Skip: one broken server, the rest keep running ([A-31]) ------------

    #[test]
    fn missing_root_drops_only_that_server() {
        let dir = TempDir::new("missing-root");
        let good_a = server(1, &["a.test"], &[8080], &dir.path);
        let bad = server(20, &["b.test"], &[8080], Path::new("/nonexistent/does/not/exist"));
        let good_b = server(40, &["c.test"], &[8081], &dir.path);

        let validated = validate(Config { servers: vec![good_a, bad, good_b] }).unwrap();

        assert_eq!(validated.servers.len(), 2, "the two valid servers must survive");
        assert_eq!(validated.issues.len(), 1);
        assert_eq!(validated.issues[0].severity, Severity::Skip);
        assert_eq!(validated.issues[0].line, 10, "points at the offending route");
        assert!(validated.issues[0].message.contains("does not exist"), "got: {:?}", validated.issues[0]);
        // The surviving servers are the ones with usable roots.
        assert!(validated.servers.iter().any(|s| s.matches_name("a.test")));
        assert!(validated.servers.iter().any(|s| s.matches_name("c.test")));
    }

    #[test]
    fn zero_body_limit_is_rejected() {
        let dir = TempDir::new("body-zero");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.client_max_body_size = 0;
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("is 0"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn body_limit_above_the_ceiling_is_rejected() {
        let dir = TempDir::new("body-huge");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.client_max_body_size = MAX_BODY_CEILING + 1;
        let mut other = server(20, &["b.test"], &[8081], &dir.path);
        other.client_max_body_size = MAX_BODY_CEILING;
        let validated = validate(Config { servers: vec![s, other] }).unwrap();
        assert_eq!(validated.servers.len(), 1, "exactly the ceiling is still allowed");
        assert!(validated.issues[0].message.contains("ceiling"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn unreadable_error_page_drops_the_server() {
        let dir = TempDir::new("bad-error-page");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.error_pages.insert(404, dir.path.join("no-such-page.html"));
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("error page for 404"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn duplicate_route_path_drops_the_server() {
        let dir = TempDir::new("dup-route");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes.push(route("/", &dir.path));
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("duplicate route path"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn route_path_must_start_with_slash() {
        let dir = TempDir::new("bad-path");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes[0].path = "uploads".to_string();
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("must start with `/`"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn invalid_redirect_status_drops_the_server() {
        let dir = TempDir::new("bad-redirect");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes[0].redirect = Some(Redirect { status: 200, target: "/elsewhere".to_string() });
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("not a redirect status"), "got: {:?}", validated.issues[0]);
    }

    /// A redirect route answers before any file lookup, so it must validate even
    /// though its root does not exist.
    #[test]
    fn redirect_route_needs_no_usable_root() {
        let dir = TempDir::new("redirect-ok");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes[0].root = PathBuf::from("/nonexistent");
        s.routes[0].redirect = Some(Redirect { status: 301, target: "/new".to_string() });
        let validated = validate(Config { servers: vec![s] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues.is_empty());
    }

    #[test]
    fn non_executable_cgi_interpreter_drops_the_server() {
        let dir = TempDir::new("cgi");
        let fake = dir.path.join("not-an-interpreter");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes[0].cgi.insert(".py".to_string(), fake);
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("not executable"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn missing_upload_dir_drops_the_server() {
        let dir = TempDir::new("upload");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.routes[0].upload_dir = Some(dir.path.join("no-such-dir"));
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("upload_dir"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn unbindable_host_drops_the_server() {
        let dir = TempDir::new("host");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.host = "not a host".to_string();
        let validated = validate(Config { servers: vec![s, server(20, &["b.test"], &[8081], &dir.path)] }).unwrap();
        assert_eq!(validated.servers.len(), 1);
        assert!(validated.issues[0].message.contains("not an IP address"), "got: {:?}", validated.issues[0]);
    }

    #[test]
    fn localhost_is_a_bindable_host() {
        let dir = TempDir::new("localhost");
        let mut s = server(1, &["a.test"], &[8080], &dir.path);
        s.host = "localhost".to_string();
        assert_eq!(validate(Config { servers: vec![s] }).unwrap().servers.len(), 1);
    }

    // --- The boundary between Skip and Fatal --------------------------------

    /// If every server is broken there is nothing left to serve, so the Skip
    /// issues are promoted to Fatal rather than silently starting a server that
    /// listens on nothing.
    #[test]
    fn all_servers_broken_is_fatal() {
        let bad_a = server(1, &["a.test"], &[8080], Path::new("/nonexistent/a"));
        let bad_b = server(20, &["b.test"], &[8081], Path::new("/nonexistent/b"));
        let fatal = validate(Config { servers: vec![bad_a, bad_b] }).unwrap_err();
        assert!(fatal.iter().all(|i| i.severity == Severity::Fatal));
        assert!(
            fatal.iter().any(|i| i.message.contains("no usable server configuration")),
            "got: {fatal:?}"
        );
    }

    /// Per-server checks run before conflict detection, so a server that has
    /// already been dropped cannot cause a fatal port conflict: it was never
    /// going to bind anything. This is what keeps [A-31] working even when the
    /// broken block shares a port with a good one.
    #[test]
    fn dropped_server_does_not_cause_a_port_conflict() {
        let dir = TempDir::new("drop-then-conflict");
        let good = server(1, &["shared.test"], &[8080], &dir.path);
        let bad = server(20, &["shared.test"], &[8080], Path::new("/nonexistent"));

        let validated = validate(Config { servers: vec![good, bad] })
            .expect("the broken duplicate should be skipped, not fatal");
        assert_eq!(validated.servers.len(), 1);
        assert_eq!(validated.issues.len(), 1);
        assert_eq!(validated.issues[0].severity, Severity::Skip);
    }

    /// End-to-end on the file the auditor will actually edit.
    #[test]
    fn shipped_config_validates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        if !super::is_executable(Path::new("/usr/bin/python3")) {
            // config.yaml registers python3 as the CGI interpreter; without it
            // installed the CGI route legitimately fails validation.
            eprintln!("skipping: /usr/bin/python3 not present");
            return;
        }
        let (config, issues) = Config::load(&root.join("config.yaml")).unwrap();
        assert!(issues.is_empty(), "shipped config should raise no warnings: {issues:?}");
        assert_eq!(config.servers.len(), 3);
        assert_eq!(config.listeners().len(), 3, "8080, 8081, 8082");
    }
}
