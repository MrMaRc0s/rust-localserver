//! Status codes to error pages.
//!
//! Two layers, and the second one is load-bearing:
//!
//! 1. The page configured for that status in the server's `error_pages` map.
//! 2. A built-in page compiled into the binary.
//!
//! The fallback is not a nicety. If a configured error page is missing,
//! unreadable, or was deleted after startup validation approved it, generating
//! the error response would itself fail — and answering *that* failure with
//! another error page would recurse. A page that always exists, needs no
//! filesystem access and cannot fail terminates that chain, which is exactly
//! the "never crash" edge an auditor pokes at ([A-12], [A-20]).

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use crate::config::ServerConfig;
use crate::http::response::{reason_phrase, Response};

/// Largest error page read from disk. A page bigger than this is a
/// misconfiguration (a video, a log file), and buffering it to answer an error
/// would be a denial-of-service vector triggered by ordinary 404s.
const MAX_ERROR_PAGE: u64 = 256 * 1024;

/// Build the response for a status, preferring the server's configured page.
pub fn page(server: &ServerConfig, status: u16) -> Response {
    if let Some(path) = server.error_pages.get(&status) {
        if let Some(body) = read_page(path) {
            return Response::with_body(status, "text/html; charset=utf-8", body);
        }
        // Configured but unusable: fall through rather than fail the request.
        eprintln!(
            "localserver: error page for {status} is unreadable ({}), using the built-in",
            path.display()
        );
    }
    builtin(status)
}

fn read_page(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ERROR_PAGE {
        return None;
    }
    std::fs::read(path).ok()
}

/// The page that always works: no filesystem, no configuration, no failure
/// mode.
pub fn builtin(status: u16) -> Response {
    let reason = reason_phrase(status);
    let explanation = explain(status);
    let body = format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{status} {reason}</title>\n\
         <style>\
         body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
         background:#10151c;color:#e6edf3;font:16px/1.6 system-ui,-apple-system,\"Segoe UI\",sans-serif}}\
         main{{max-width:32rem;padding:2rem;text-align:center}}\
         .code{{font-size:4rem;font-weight:700;color:#58a6ff;line-height:1}}\
         h1{{font-size:1.25rem;margin:.5rem 0 1rem}}p{{color:#8b98a5}}a{{color:#58a6ff}}\
         </style></head>\n\
         <body><main><div class=\"code\">{status}</div><h1>{reason}</h1>\
         <p>{explanation}</p><p><a href=\"/\">Back to the index</a></p>\
         </main></body></html>\n"
    );
    Response::with_body(status, "text/html; charset=utf-8", body.into_bytes())
}

/// A sentence explaining the status, so the page is useful rather than just
/// decorative.
fn explain(status: u16) -> &'static str {
    match status {
        400 => "The request could not be parsed. Check the request line, the headers, and that a Host header is present.",
        403 => "Access to this resource is refused. It may be outside the configured route root, or a directory with listing disabled.",
        404 => "No route or file matches this URL on this server.",
        405 => "This route does not accept that HTTP method. The Allow header lists the ones it does.",
        408 => "The request was not completed in time and the connection was closed.",
        411 => "This request needs a Content-Length header.",
        413 => "The request body exceeded this server's configured client_max_body_size.",
        414 => "The request target is longer than this server accepts.",
        415 => "The request body is in a format this resource does not accept.",
        431 => "The request header block is larger than this server accepts.",
        500 => "The server hit an unexpected condition while handling the request.",
        501 => "The server does not implement the functionality required to fulfil this request.",
        502 => "A CGI script produced no usable response.",
        504 => "A CGI script took too long and was stopped.",
        505 => "This server speaks HTTP/1.0 and HTTP/1.1 only.",
        s if (400..500).contains(&s) => "The request could not be fulfilled.",
        _ => "The server could not fulfil the request.",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn server(error_pages: BTreeMap<u16, PathBuf>) -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            ports: vec![8080],
            server_names: Vec::new(),
            is_default: true,
            client_max_body_size: 1024,
            error_pages,
            routes: Vec::new(),
            line: 1,
        }
    }

    #[test]
    fn uses_the_configured_page_when_it_is_readable() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("error_pages/404.html");
        let mut pages = BTreeMap::new();
        pages.insert(404, path);
        let response = page(&server(pages), 404);
        assert_eq!(response.status, 404);
        let body = String::from_utf8_lossy(&response.body).to_string();
        assert!(body.contains("404"), "should be the configured page");
        assert!(body.contains("Back to the index"));
    }

    /// The case the whole two-layer design exists for: a page that validation
    /// approved but that has since gone away must not fail the request.
    #[test]
    fn falls_back_when_the_configured_page_is_missing() {
        let mut pages = BTreeMap::new();
        pages.insert(404, PathBuf::from("/nonexistent/404.html"));
        let response = page(&server(pages), 404);
        assert_eq!(response.status, 404);
        assert!(!response.body.is_empty(), "the built-in page must still render");
        assert!(String::from_utf8_lossy(&response.body).contains("Not Found"));
    }

    #[test]
    fn falls_back_when_no_page_is_configured() {
        let response = page(&server(BTreeMap::new()), 500);
        assert_eq!(response.status, 500);
        assert!(String::from_utf8_lossy(&response.body).contains("Internal Server Error"));
    }

    /// An oversized "error page" would turn every 404 into a large allocation.
    #[test]
    fn refuses_an_absurdly_large_configured_page() {
        let big = std::env::temp_dir().join(format!("localserver-bigpage-{}", std::process::id()));
        std::fs::write(&big, vec![b'x'; (MAX_ERROR_PAGE + 1) as usize]).unwrap();
        let mut pages = BTreeMap::new();
        pages.insert(404, big.clone());
        let response = page(&server(pages), 404);
        assert!(
            String::from_utf8_lossy(&response.body).contains("Not Found"),
            "should have fallen back to the built-in"
        );
        let _ = std::fs::remove_file(&big);
    }

    /// The subject's mandatory list, plus the ones this server can emit.
    #[test]
    fn every_required_status_has_a_built_in_page() {
        for status in [400u16, 403, 404, 405, 408, 413, 500, 501, 505] {
            let response = builtin(status);
            assert_eq!(response.status, status);
            let body = String::from_utf8_lossy(&response.body).to_string();
            assert!(body.contains(&status.to_string()), "{status} missing its code");
            assert!(body.contains(reason_phrase(status)), "{status} missing its reason");
            assert!(
                body.len() > 200,
                "{status} should be a real page, not a stub"
            );
        }
    }

    #[test]
    fn built_in_pages_are_self_contained() {
        let body = String::from_utf8_lossy(&builtin(404).body).to_string();
        // No external stylesheet or image: the page must render even when
        // nothing else on the server works.
        assert!(!body.contains("<link"));
        assert!(!body.contains("<img"));
        assert!(body.contains("<style>"));
    }
}
