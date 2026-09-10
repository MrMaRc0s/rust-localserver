//! Resolution of `(Host, path, method)` to a concrete action.
//!
//! # Virtual host selection ([A-11])
//!
//! Several servers may share one listening socket. The `Host` header — port
//! stripped, lowercased — picks between them; if it matches no `server_names`,
//! the server marked `is_default` for that address answers, and failing that
//! the first one declared. That is what lets
//! `curl --resolve test.com:8080:127.0.0.1 http://test.com:8080/` reach a
//! different site than `curl http://127.0.0.1:8080/` on the same socket.
//!
//! # Route matching order, and why it is what it is ([A-14]..[A-16], [A-27])
//!
//! Routes are matched by **longest prefix**, so `/uploads` wins over `/` for
//! `/uploads/a.txt`. Within the winning route the order is deliberate and the
//! audit asks about it:
//!
//! 1. **Redirect first.** A redirect route is not serving the resource at all,
//!    so checking methods or looking for a file would be answering a question
//!    nobody asked. `/old-page` redirects whether or not `old-page` exists.
//! 2. **Method check before anything filesystem-related.** 405 precedes 404
//!    because whether the resource exists is not the question being answered —
//!    and revealing existence to a client that may not use that method is a
//!    small information leak.
//! 3. **Path sanitising**, before any `metadata` call (see [`crate::utils::path`]).
//! 4. **CGI by extension**, before the directory rules, so a script is executed
//!    rather than served as a download.
//! 5. **Directory rules**: configured `index` file if present, else a listing if
//!    `autoindex` is on, else 403 — not 404, because the directory does exist;
//!    the client is simply not allowed to enumerate it.
//! 6. **Otherwise serve the file**, or 404 if it is not there.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use crate::config::{Method, RouteConfig, ServerConfig};
use crate::http::request::Request;
use crate::utils::path as pathutil;

/// What the handlers (TASKS.md phase 5) should do with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Answer a 3xx with this `Location`.
    Redirect { status: u16, location: String },
    /// Send this file.
    File { path: PathBuf },
    /// Generate a directory listing. `url_path` is needed to build the links.
    Listing { path: PathBuf, url_path: String },
    /// Execute a CGI script.
    Cgi {
        interpreter: PathBuf,
        script: PathBuf,
        /// The URL portion after the script name, per RFC 3875.
        path_info: String,
    },
    /// Store the request body under this directory.
    Upload { dir: PathBuf },
    /// Remove this file.
    Delete { path: PathBuf },
    /// The route exists but does not accept this method. `allow` is the
    /// `Allow` header's value, which RFC 9110 requires on a 405.
    MethodNotAllowed { allow: String },
    /// Answer this status directly.
    Error(u16),
}

/// Pick the server that should answer, from those sharing a listener.
pub fn select_server<'a>(
    servers: &'a [ServerConfig],
    host: Option<&str>,
) -> Option<&'a ServerConfig> {
    if let Some(host) = host {
        if let Some(server) = servers.iter().find(|s| s.matches_name(host)) {
            return Some(server);
        }
    }
    // No name matched: the default server for this address answers, so that an
    // unknown Host still gets a sensible site rather than an error.
    servers
        .iter()
        .find(|s| s.is_default)
        .or_else(|| servers.first())
}

/// Longest-prefix route match.
///
/// A prefix only matches at a segment boundary: `/upload` must not capture
/// `/uploads`, which is why this is not a plain `starts_with`.
pub fn match_route<'a>(server: &'a ServerConfig, path: &str) -> Option<&'a RouteConfig> {
    let decoded = pathutil::percent_decode(path).unwrap_or_else(|_| path.to_string());
    server
        .routes
        .iter()
        .filter(|route| prefix_matches(&route.path, &decoded))
        .max_by_key(|route| route.path.trim_end_matches('/').len())
}

fn prefix_matches(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        // The "/" route matches everything.
        return true;
    }
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/')
}

/// The part of the URL path below the route's prefix.
fn remainder_of(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let rest = path.get(prefix.len()..).unwrap_or("");
    if rest.is_empty() {
        "/".to_string()
    } else {
        rest.to_string()
    }
}

/// The `Allow` header value for a route.
fn allow_header(route: &RouteConfig) -> String {
    route
        .methods
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Find the first path segment whose extension is registered for CGI.
///
/// Everything after it is `PATH_INFO`: for `/cgi-bin/app.py/users/7`, the
/// script is `app.py` and `PATH_INFO` is `/users/7`. Resolving the whole URL to
/// one file instead would 404 on a script that takes a path.
fn split_cgi_target(route: &RouteConfig, segments: &[String]) -> Option<(usize, PathBuf)> {
    for (index, segment) in segments.iter().enumerate() {
        let extension = match segment.rfind('.') {
            Some(dot) => &segment[dot..],
            None => continue,
        };
        if let Some(interpreter) = route.cgi.get(extension) {
            return Some((index, interpreter.clone()));
        }
    }
    None
}

/// Resolve a request against the server that will answer it.
pub fn resolve(server: &ServerConfig, request: &Request) -> Resolution {
    let decoded_path = match pathutil::percent_decode(&request.path) {
        Ok(decoded) => decoded,
        Err(status) => return Resolution::Error(status),
    };
    let route = match match_route(server, &decoded_path) {
        Some(route) => route,
        // No route claims this prefix at all.
        None => return Resolution::Error(404),
    };

    // 1. A redirect answers before anything else is considered.
    if let Some(redirect) = &route.redirect {
        return Resolution::Redirect {
            status: redirect.status,
            location: redirect.target.clone(),
        };
    }

    // 2. Method, before the filesystem is touched.
    let method = match request.method_enum() {
        Some(method) => method,
        // Well formed but not implemented by this server.
        None => return Resolution::Error(501),
    };
    if !route.methods.contains(&method) {
        return Resolution::MethodNotAllowed { allow: allow_header(route) };
    }

    // 3. Sanitise before resolving anything on disk.
    let remainder = remainder_of(&route.path, &decoded_path);
    let segments = match pathutil::normalize_segments(&remainder) {
        Ok(segments) => segments,
        Err(status) => return Resolution::Error(status),
    };

    // 4. CGI, before the directory rules, so a script runs rather than being
    //    served as a file.
    if let Some((index, interpreter)) = split_cgi_target(route, &segments) {
        let script_segments = &segments[..=index];
        let mut script = route.root.clone();
        for segment in script_segments {
            script.push(segment);
        }
        if let Err(status) = confirm_under(&route.root, &script) {
            return Resolution::Error(status);
        }
        if !script.is_file() {
            return Resolution::Error(404);
        }
        let path_info = if index + 1 < segments.len() {
            format!("/{}", segments[index + 1..].join("/"))
        } else {
            String::new()
        };
        return Resolution::Cgi { interpreter, script, path_info };
    }

    let target = match pathutil::resolve_segments_under(&route.root, &segments) {
        Ok(target) => target,
        Err(status) => return Resolution::Error(status),
    };

    let metadata = std::fs::metadata(&target).ok();
    let is_directory = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);

    // 5. Directory rules.
    if is_directory {
        // Without the trailing slash every relative link in the served page
        // resolves one level too high, so browsing a nested directory breaks.
        if !request.path.ends_with('/') {
            return Resolution::Redirect {
                status: 301,
                location: pathutil::with_trailing_slash(&request.path, &request.query),
            };
        }

        if let Some(index) = &route.index {
            let candidate = target.join(index);
            if candidate.is_file() {
                return finish_file(method, route, candidate);
            }
        }

        return match method {
            Method::Get | Method::Head if route.autoindex => Resolution::Listing {
                path: target,
                url_path: request.path.clone(),
            },
            // The directory exists; the client just may not enumerate it. 403
            // rather than 404, which would be a lie.
            Method::Get | Method::Head => Resolution::Error(403),
            Method::Post => match &route.upload_dir {
                Some(dir) => Resolution::Upload { dir: dir.clone() },
                None => Resolution::Error(403),
            },
            // Deleting a directory tree over HTTP is not something this server
            // offers, however the route is configured.
            Method::Delete => Resolution::Error(403),
            Method::Put | Method::Options => Resolution::Error(501),
        };
    }

    // 6. A concrete file (or a missing one).
    finish_file(method, route, target)
}

/// Decide what to do with a resolved, non-directory target.
fn finish_file(method: Method, route: &RouteConfig, target: PathBuf) -> Resolution {
    let exists = target.is_file();
    match method {
        Method::Get | Method::Head => {
            if exists {
                Resolution::File { path: target }
            } else {
                Resolution::Error(404)
            }
        }
        Method::Post => match &route.upload_dir {
            Some(dir) => Resolution::Upload { dir: dir.clone() },
            // POST is accepted by the route but there is nowhere to put a body
            // and no script to hand it to.
            None => Resolution::Error(403),
        },
        Method::Delete => {
            if exists {
                Resolution::Delete { path: target }
            } else {
                Resolution::Error(404)
            }
        }
        Method::Put | Method::Options => Resolution::Error(501),
    }
}

/// Traversal check for a path built segment by segment (the CGI case, which
/// does not go through `resolve_under`).
fn confirm_under(root: &Path, candidate: &Path) -> Result<(), u16> {
    let real_root = root.canonicalize().map_err(|_| 500u16)?;
    match candidate.canonicalize() {
        Ok(real) if real.starts_with(&real_root) => Ok(()),
        Ok(_) => Err(403),
        // Not existing is fine here; the caller answers 404.
        Err(_) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tests — audit question [A-38] (route matching) plus the status codes those
// decisions produce: [A-14], [A-15], [A-16], [A-27].
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::config::Redirect;
    use crate::http::request::{Progress, RequestParser};
    use std::collections::BTreeMap;

    /// The repository's own `www/` tree, so route resolution is exercised
    /// against real files rather than a mock filesystem.
    fn www() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("www")
    }

    fn route(path: &str, root: PathBuf, methods: &[Method]) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            methods: methods.to_vec(),
            root,
            index: None,
            autoindex: false,
            redirect: None,
            cgi: BTreeMap::new(),
            upload_dir: None,
            line: 1,
        }
    }

    fn server(names: &[&str], routes: Vec<RouteConfig>) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            ports: vec![8080],
            server_names: names.iter().map(|n| n.to_string()).collect(),
            is_default: false,
            client_max_body_size: 1024 * 1024,
            error_pages: BTreeMap::new(),
            routes,
            line: 1,
        }
    }

    fn request(raw: &str) -> Request {
        let mut parser = RequestParser::new(64 * 1024);
        assert_eq!(parser.feed(raw.as_bytes()), Progress::Complete, "test request must parse");
        match parser.take() {
            Some(request) => request,
            None => panic!("test request did not complete"),
        }
    }

    fn get(path: &str) -> Request {
        request(&format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n"))
    }

    // --- virtual host selection ([A-11]) ------------------------------------

    #[test]
    fn selects_the_server_whose_name_matches_host() {
        let servers = vec![
            server(&["first.test"], vec![route("/", www(), &[Method::Get])]),
            server(&["second.test"], vec![route("/", www(), &[Method::Get])]),
        ];
        let selected = select_server(&servers, Some("second.test")).unwrap();
        assert_eq!(selected.server_names, vec!["second.test"]);
    }

    #[test]
    fn host_matching_ignores_case() {
        let servers = vec![server(&["site.test"], vec![route("/", www(), &[Method::Get])])];
        assert!(select_server(&servers, Some("SITE.TEST")).is_some());
    }

    /// An unknown Host is not an error: the default server answers, which is
    /// what makes `curl http://127.0.0.1:8080/` work at all.
    #[test]
    fn an_unmatched_host_falls_back_to_the_default_server() {
        let mut fallback = server(&["fallback.test"], vec![route("/", www(), &[Method::Get])]);
        fallback.is_default = true;
        let servers = vec![
            server(&["other.test"], vec![route("/", www(), &[Method::Get])]),
            fallback,
        ];
        let selected = select_server(&servers, Some("nobody.knows")).unwrap();
        assert_eq!(selected.server_names, vec!["fallback.test"]);
        // A request with no Host at all lands there too.
        assert_eq!(
            select_server(&servers, None).unwrap().server_names,
            vec!["fallback.test"]
        );
    }

    #[test]
    fn without_a_default_the_first_server_answers() {
        let servers = vec![
            server(&["a.test"], vec![route("/", www(), &[Method::Get])]),
            server(&["b.test"], vec![route("/", www(), &[Method::Get])]),
        ];
        assert_eq!(
            select_server(&servers, Some("unknown")).unwrap().server_names,
            vec!["a.test"]
        );
    }

    // --- route matching ([A-14]) --------------------------------------------

    #[test]
    fn longest_prefix_wins() {
        let s = server(
            &[],
            vec![
                route("/", www(), &[Method::Get]),
                route("/listing", www().join("listing"), &[Method::Get]),
                route("/listing/nested", www().join("listing/nested"), &[Method::Get]),
            ],
        );
        assert_eq!(match_route(&s, "/index.html").unwrap().path, "/");
        assert_eq!(match_route(&s, "/listing/notes.txt").unwrap().path, "/listing");
        assert_eq!(match_route(&s, "/listing/nested/deep.txt").unwrap().path, "/listing/nested");
    }

    /// A prefix must only match at a segment boundary, or `/upload` would
    /// capture `/uploads` and serve it from the wrong root.
    #[test]
    fn a_prefix_only_matches_at_a_segment_boundary() {
        assert!(prefix_matches("/uploads", "/uploads"));
        assert!(prefix_matches("/uploads", "/uploads/a.txt"));
        assert!(!prefix_matches("/uploads", "/uploadsomething"));
        assert!(prefix_matches("/", "/anything/at/all"));
    }

    #[test]
    fn no_matching_route_is_404() {
        let s = server(&[], vec![route("/files", www(), &[Method::Get])]);
        assert_eq!(resolve(&s, &get("/elsewhere")), Resolution::Error(404));
    }

    // --- ordering: redirect, then method, then filesystem -------------------

    /// [A-27]. A redirect answers before the method is even considered: the
    /// route is not serving the resource, so nothing else is relevant.
    #[test]
    fn a_redirect_answers_before_anything_else() {
        let mut r = route("/old", PathBuf::from("/nonexistent"), &[Method::Get]);
        r.redirect = Some(Redirect { status: 301, target: "/new".to_string() });
        let s = server(&[], vec![r]);

        assert_eq!(
            resolve(&s, &get("/old")),
            Resolution::Redirect { status: 301, location: "/new".to_string() }
        );
        // Even for a method the route does not list, and a root that is missing.
        let delete = request("DELETE /old HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(matches!(resolve(&s, &delete), Resolution::Redirect { .. }));
    }

    /// [A-16]. The same directory behind two routes, one allowing DELETE and
    /// one not — exactly the shape `config.yaml` ships for the audit.
    #[test]
    fn method_not_in_the_route_is_405_with_an_allow_header() {
        let s = server(
            &[],
            vec![
                route("/rw", www(), &[Method::Get, Method::Delete]),
                route("/ro", www(), &[Method::Get]),
            ],
        );
        let delete_ro = request("DELETE /ro/index.html HTTP/1.1\r\nHost: x\r\n\r\n");
        match resolve(&s, &delete_ro) {
            Resolution::MethodNotAllowed { allow } => assert_eq!(allow, "GET"),
            other => panic!("expected 405, got {other:?}"),
        }
        // The same file through the permissive route is deletable.
        let delete_rw = request("DELETE /rw/index.html HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(matches!(resolve(&s, &delete_rw), Resolution::Delete { .. }));
    }

    /// 405 must precede 404: whether the resource exists is not the question
    /// being answered, and revealing it would leak information.
    #[test]
    fn method_rejection_precedes_the_existence_check() {
        let s = server(&[], vec![route("/ro", www(), &[Method::Get])]);
        let delete_missing = request("DELETE /ro/no-such-file HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(matches!(
            resolve(&s, &delete_missing),
            Resolution::MethodNotAllowed { .. }
        ));
    }

    #[test]
    fn an_unimplemented_method_is_501() {
        let s = server(&[], vec![route("/", www(), &[Method::Get])]);
        let brew = request("BREW /pot HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(resolve(&s, &brew), Resolution::Error(501));
    }

    // --- files and directories ([A-15]) -------------------------------------

    #[test]
    fn an_existing_file_resolves_to_itself() {
        let s = server(&[], vec![route("/", www(), &[Method::Get])]);
        assert_eq!(
            resolve(&s, &get("/index.html")),
            Resolution::File { path: www().join("index.html") }
        );
    }

    #[test]
    fn a_missing_file_is_404() {
        let s = server(&[], vec![route("/", www(), &[Method::Get])]);
        assert_eq!(resolve(&s, &get("/no-such-file.html")), Resolution::Error(404));
    }

    /// [A-15]: a directory with a configured index serves that file.
    #[test]
    fn a_directory_serves_its_configured_index() {
        let mut r = route("/", www(), &[Method::Get]);
        r.index = Some("index.html".to_string());
        let s = server(&[], vec![r]);
        assert_eq!(
            resolve(&s, &get("/")),
            Resolution::File { path: www().join("index.html") }
        );
    }

    #[test]
    fn a_directory_without_index_and_without_autoindex_is_403() {
        let mut r = route("/", www().join("listing"), &[Method::Get]);
        r.autoindex = false;
        let s = server(&[], vec![r]);
        // 403, not 404: the directory exists, the client may not enumerate it.
        assert_eq!(resolve(&s, &get("/")), Resolution::Error(403));
    }

    #[test]
    fn a_directory_with_autoindex_is_listed() {
        let mut r = route("/", www().join("listing"), &[Method::Get]);
        r.autoindex = true;
        let s = server(&[], vec![r]);
        assert_eq!(
            resolve(&s, &get("/")),
            Resolution::Listing { path: www().join("listing"), url_path: "/".to_string() }
        );
    }

    /// Without the trailing slash, every relative link on the page resolves one
    /// level too high, so nested browsing breaks in a real browser.
    #[test]
    fn a_directory_without_a_trailing_slash_redirects() {
        let mut r = route("/", www(), &[Method::Get]);
        r.autoindex = true;
        let s = server(&[], vec![r]);
        assert_eq!(
            resolve(&s, &get("/listing")),
            Resolution::Redirect { status: 301, location: "/listing/".to_string() }
        );
        // The query string survives the redirect.
        assert_eq!(
            resolve(&s, &get("/listing?sort=name")),
            Resolution::Redirect { status: 301, location: "/listing/?sort=name".to_string() }
        );
    }

    // --- traversal ([A-25] and the path tests) -------------------------------

    #[test]
    fn traversal_attempts_are_403() {
        let s = server(&[], vec![route("/", www(), &[Method::Get])]);
        assert_eq!(resolve(&s, &get("/../Cargo.toml")), Resolution::Error(403));
        assert_eq!(resolve(&s, &get("/%2e%2e/Cargo.toml")), Resolution::Error(403));
        assert_eq!(resolve(&s, &get("/../../etc/passwd")), Resolution::Error(403));
    }

    #[test]
    fn an_encoded_nul_is_400() {
        let s = server(&[], vec![route("/", www(), &[Method::Get])]);
        assert_eq!(resolve(&s, &get("/index.html%00.txt")), Resolution::Error(400));
    }

    // --- CGI ----------------------------------------------------------------

    #[test]
    fn an_extension_registered_for_cgi_selects_its_interpreter() {
        let cgi_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("cgi-bin");
        let script = cgi_root.join("router-test.py");
        std::fs::write(&script, b"#!/usr/bin/env python3\nprint()\n").unwrap();

        let mut r = route("/cgi-bin", cgi_root.clone(), &[Method::Get, Method::Post]);
        r.cgi.insert(".py".to_string(), PathBuf::from("/usr/bin/python3"));
        let s = server(&[], vec![r]);

        match resolve(&s, &get("/cgi-bin/router-test.py")) {
            Resolution::Cgi { interpreter, script: resolved, path_info } => {
                assert_eq!(interpreter, PathBuf::from("/usr/bin/python3"));
                assert_eq!(resolved, script);
                assert_eq!(path_info, "");
            }
            other => panic!("expected CGI, got {other:?}"),
        }

        // Everything after the script name is PATH_INFO, not part of the path
        // to open — a script taking a path would otherwise 404.
        match resolve(&s, &get("/cgi-bin/router-test.py/users/7")) {
            Resolution::Cgi { path_info, .. } => assert_eq!(path_info, "/users/7"),
            other => panic!("expected CGI with PATH_INFO, got {other:?}"),
        }

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn an_unregistered_extension_is_served_as_a_file() {
        let mut r = route("/", www(), &[Method::Get]);
        r.cgi.insert(".py".to_string(), PathBuf::from("/usr/bin/python3"));
        let s = server(&[], vec![r]);
        // .html is not registered, so it is a plain file.
        assert_eq!(
            resolve(&s, &get("/index.html")),
            Resolution::File { path: www().join("index.html") }
        );
    }

    #[test]
    fn a_missing_cgi_script_is_404() {
        let cgi_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("cgi-bin");
        let mut r = route("/cgi-bin", cgi_root, &[Method::Get]);
        r.cgi.insert(".py".to_string(), PathBuf::from("/usr/bin/python3"));
        let s = server(&[], vec![r]);
        assert_eq!(resolve(&s, &get("/cgi-bin/absent.py")), Resolution::Error(404));
    }

    // --- uploads and deletes -------------------------------------------------

    #[test]
    fn post_to_a_route_with_an_upload_dir_resolves_to_upload() {
        let uploads = Path::new(env!("CARGO_MANIFEST_DIR")).join("uploads");
        let mut r = route("/uploads", uploads.clone(), &[Method::Get, Method::Post]);
        r.upload_dir = Some(uploads.clone());
        let s = server(&[], vec![r]);
        let post = request("POST /uploads/ HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\nhi");
        assert_eq!(resolve(&s, &post), Resolution::Upload { dir: uploads });
    }

    #[test]
    fn post_to_a_route_with_nowhere_to_put_it_is_403() {
        let s = server(&[], vec![route("/", www(), &[Method::Get, Method::Post])]);
        let post = request("POST /index.html HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\nhi");
        assert_eq!(resolve(&s, &post), Resolution::Error(403));
    }

    #[test]
    fn deleting_a_missing_file_is_404_and_a_directory_is_403() {
        let s = server(&[], vec![route("/", www(), &[Method::Get, Method::Delete])]);
        let missing = request("DELETE /no-such-file HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(resolve(&s, &missing), Resolution::Error(404));
        let directory = request("DELETE /listing/ HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(resolve(&s, &directory), Resolution::Error(403));
    }
}
