//! The `/session` demo page.
//!
//! Sessions are invisible by nature: the audit's "show me a working session and
//! cookie system" ([A-22]) is answered by opening devtools → Application →
//! Cookies and watching the value persist across requests. This page makes the
//! same thing visible without devtools — the id, the visit counter, the expiry
//! countdown, and a name that survives a reload.
//!
//! These paths are built in rather than configured, like the metrics endpoint
//! in TASKS.md 8.1: they report on the server itself, so there is no document
//! root they could sensibly be mapped to. They are checked before routing, and
//! the prefix is a single constant so it can be moved in one place.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::time::{SystemTime, UNIX_EPOCH};

use crate::handlers::Reply;
use crate::http::request::Request;
use crate::http::response::{http_date, Response};
use crate::utils::path::percent_decode;
use crate::utils::session::{Session, SessionStore};

/// URL prefix owned by the session demo.
pub const PREFIX: &str = "/session";

/// Does this request belong to the demo?
pub fn owns(path: &str) -> bool {
    path == PREFIX || path.starts_with("/session/")
}

/// What the demo was asked to do.
pub enum Command {
    Show,
    /// Store this name on the session.
    Login(String),
    /// Destroy the session.
    Logout,
    /// A path under `/session/` that means nothing.
    Unknown,
}

/// Work out the command without touching the store.
pub fn command(request: &Request) -> Command {
    match request.path.trim_end_matches('/') {
        "/session" | "" => Command::Show,
        "/session/login" => match form_value(&request.body, "name") {
            Some(name) => Command::Login(name),
            None => Command::Show,
        },
        "/session/logout" => Command::Logout,
        _ => Command::Unknown,
    }
}

/// Read one field out of an `application/x-www-form-urlencoded` body.
fn form_value(body: &[u8], field: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    for pair in text.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key != field {
            continue;
        }
        // `+` means space in form encoding, which percent-decoding alone does
        // not handle.
        let value = value.replace('+', " ");
        let decoded = percent_decode(&value).ok()?;
        let trimmed = decoded.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Bound it: this goes into a page and into the store.
        return Some(trimmed.chars().take(64).collect());
    }
    None
}

/// Render the demo page for a session.
pub fn page(id: &str, session: &Session, store: &SessionStore, is_new: bool) -> Reply {
    let now = std::time::Instant::now();
    let created = created_at(session);
    let remaining = session.remaining(now);
    let greeting = match session.name() {
        Some(name) => format!(
            "<p class=\"greeting\">Hello, <strong>{}</strong>.</p>",
            escape(name)
        ),
        None => "<p class=\"greeting\">Nobody is signed in on this session.</p>".to_string(),
    };
    let status = if is_new {
        "<p class=\"badge new\">New session — a Set-Cookie header came with this response.</p>"
    } else {
        "<p class=\"badge\">Existing session — your browser sent the cookie back.</p>"
    };

    let body = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Session — LocalServer</title>\n\
         <style>\
         body{{margin:0;padding:2.5rem 1.5rem;background:#10151c;color:#e6edf3;\
         font:16px/1.6 system-ui,-apple-system,\"Segoe UI\",sans-serif}}\
         main{{max-width:44rem;margin:0 auto}}\
         h1{{font-size:1.5rem;margin:0 0 .25rem}}\
         .lead{{color:#8b98a5;margin:0 0 1.5rem}}\
         .badge{{display:inline-block;padding:.3rem .7rem;border-radius:999px;\
         background:#182029;border:1px solid #2a3441;color:#8b98a5;font-size:.85rem}}\
         .badge.new{{border-color:#58a6ff;color:#58a6ff}}\
         .greeting{{font-size:1.1rem}}\
         table{{width:100%;border-collapse:collapse;margin:1.5rem 0}}\
         th,td{{text-align:left;padding:.45rem .6rem;border-bottom:1px solid #2a3441;\
         vertical-align:top}}\
         th{{color:#8b98a5;font-weight:600;width:11rem}}\
         code{{font-family:ui-monospace,Menlo,monospace;font-size:.85em;word-break:break-all}}\
         form{{display:flex;gap:.5rem;margin:1rem 0;flex-wrap:wrap}}\
         input,button{{font:inherit;padding:.45rem .7rem;border-radius:6px;\
         border:1px solid #2a3441;background:#182029;color:#e6edf3}}\
         button{{cursor:pointer;border-color:#58a6ff;color:#58a6ff}}\
         a{{color:#58a6ff}}\
         footer{{margin-top:2rem;padding-top:1rem;border-top:1px solid #2a3441;\
         color:#8b98a5;font-size:.85rem}}\
         </style></head>\n<body><main>\n\
         <h1>Session</h1>\n\
         <p class=\"lead\">Reload this page and watch the visit counter climb. \
         The cookie is visible in devtools under Application → Cookies.</p>\n\
         {status}\n{greeting}\n\
         <table>\n\
         <tr><th>Session id</th><td><code>{id}</code></td></tr>\n\
         <tr><th>Cookie name</th><td><code>{cookie}</code></td></tr>\n\
         <tr><th>Requests on it</th><td>{visits}</td></tr>\n\
         <tr><th>Created</th><td>{created}</td></tr>\n\
         <tr><th>Expires in</th><td>{remaining} s (sliding — each request resets it)</td></tr>\n\
         <tr><th>Signed in as</th><td>{name}</td></tr>\n\
         <tr><th>Sessions on server</th><td>{total}</td></tr>\n\
         </table>\n\
         <form method=\"POST\" action=\"/session/login\">\n\
         <input type=\"text\" name=\"name\" placeholder=\"Your name\" maxlength=\"64\">\n\
         <button type=\"submit\">Sign in</button>\n\
         </form>\n\
         <p><a href=\"/session/logout\">Sign out and start a new session</a> · \
         <a href=\"/\">Back to the index</a></p>\n\
         <footer>The cookie is HttpOnly and SameSite=Lax, so scripts on this page \
         cannot read it. LocalServer.</footer>\n\
         </main></body></html>\n",
        status = status,
        greeting = greeting,
        id = escape(id),
        cookie = crate::utils::session::COOKIE_NAME,
        visits = session.visits,
        created = created,
        remaining = remaining,
        name = session
            .name()
            .map(escape)
            .unwrap_or_else(|| "—".to_string()),
        total = store.len(),
    );

    Reply::immediate(Response::with_body(
        200,
        "text/html; charset=utf-8",
        body.into_bytes(),
    ))
}

/// The page shown when sessions could not be issued at all.
pub fn unavailable() -> Reply {
    Reply::immediate(Response::with_body(
        503,
        "text/html; charset=utf-8",
        b"<!doctype html><html><head><meta charset=\"utf-8\"><title>Sessions unavailable</title>\
          </head><body><h1>Sessions unavailable</h1>\
          <p>The server could not generate a secure session id, so it is not issuing sessions. \
          Everything else still works.</p></body></html>\n"
            .to_vec(),
    ))
}

/// A redirect back to the demo page, used after login and logout.
///
/// 303 rather than 302: it tells the browser to follow up with a GET, so a
/// reload after signing in does not re-post the form.
pub fn redirect_to_self() -> Reply {
    Reply::immediate(
        Response::with_body(
            303,
            "text/plain; charset=utf-8",
            b"See /session\n".to_vec(),
        )
        .header("Location", PREFIX),
    )
}

fn created_at(session: &Session) -> String {
    match session.created.duration_since(UNIX_EPOCH) {
        Ok(_) => http_date(session.created),
        Err(_) => http_date(SystemTime::now()),
    }
}

fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::http::request::{Progress, RequestParser};
    use crate::utils::session::DEFAULT_TTL;

    fn request(raw: &[u8]) -> Request {
        let mut parser = RequestParser::new(4096);
        assert_eq!(parser.feed(raw), Progress::Complete);
        parser.take().unwrap()
    }

    #[test]
    fn claims_only_its_own_paths() {
        assert!(owns("/session"));
        assert!(owns("/session/"));
        assert!(owns("/session/login"));
        // A file whose name merely starts the same way is not ours.
        assert!(!owns("/sessions"));
        assert!(!owns("/sessionary.html"));
        assert!(!owns("/"));
    }

    #[test]
    fn reads_a_name_out_of_a_form_body() {
        assert_eq!(form_value(b"name=Ada", "name").as_deref(), Some("Ada"));
        // `+` is a space in form encoding.
        assert_eq!(
            form_value(b"name=Ada+Lovelace", "name").as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(
            form_value(b"name=Ada%20Lovelace&x=1", "name").as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(form_value(b"other=1", "name"), None);
        assert_eq!(form_value(b"name=", "name"), None);
        assert_eq!(form_value(b"name=%20%20", "name"), None, "whitespace is not a name");
    }

    #[test]
    fn bounds_the_stored_name() {
        let long = format!("name={}", "a".repeat(500));
        let value = form_value(long.as_bytes(), "name").unwrap();
        assert_eq!(value.chars().count(), 64);
    }

    #[test]
    fn recognises_the_commands() {
        assert!(matches!(command(&request(b"GET /session HTTP/1.1\r\nHost: x\r\n\r\n")), Command::Show));
        assert!(matches!(command(&request(b"GET /session/ HTTP/1.1\r\nHost: x\r\n\r\n")), Command::Show));
        assert!(matches!(
            command(&request(b"GET /session/logout HTTP/1.1\r\nHost: x\r\n\r\n")),
            Command::Logout
        ));
        assert!(matches!(
            command(&request(b"GET /session/nowhere HTTP/1.1\r\nHost: x\r\n\r\n")),
            Command::Unknown
        ));

        let login = request(b"POST /session/login HTTP/1.1\r\nHost: x\r\nContent-Length: 8\r\n\r\nname=Ada");
        match command(&login) {
            Command::Login(name) => assert_eq!(name, "Ada"),
            _ => panic!("expected a login"),
        }
    }

    /// A name is user input that lands in a page; it must not be able to carry
    /// markup with it.
    #[test]
    fn the_page_escapes_a_stored_name() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        let id = store.create().unwrap();
        store.set_value(&id, "name", "<script>alert(1)</script>");
        let session = store.get(&id).unwrap().clone();
        let reply = page(&id, &session, &store, false);
        let html = String::from_utf8_lossy(&reply.response.body).to_string();
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_page_shows_the_visit_counter_and_id() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        let id = store.create().unwrap();
        store.touch(&id);
        let session = store.get(&id).unwrap().clone();
        let reply = page(&id, &session, &store, true);
        let html = String::from_utf8_lossy(&reply.response.body).to_string();
        assert_eq!(reply.response.status, 200);
        assert!(html.contains(&id), "the id should be visible on the page");
        assert!(html.contains("New session"));
        assert!(html.contains(crate::utils::session::COOKIE_NAME));
    }

    /// 303 makes the browser follow up with a GET, so reloading after signing
    /// in does not re-post the form.
    #[test]
    fn the_post_login_redirect_is_a_see_other() {
        let reply = redirect_to_self();
        assert_eq!(reply.response.status, 303);
        assert_eq!(reply.response.get_header("Location"), Some("/session"));
    }
}
