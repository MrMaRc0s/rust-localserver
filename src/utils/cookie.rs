//! `Cookie:` header parsing and `Set-Cookie:` construction.
//!
//! Two different grammars, deliberately kept apart. A request's `Cookie` header
//! is a flat `name=value; name=value` list with no attributes — a client sends
//! back only the pairs, never the `Path` or `HttpOnly` it was given. A
//! `Set-Cookie` response header carries exactly one pair plus its attributes.
//! Treating them as one format is a common source of confusion.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::fmt;

/// How a cookie is offered to cross-site requests.
///
/// Only `Lax` is used by this server's own cookies; the other two exist so the
/// builder is complete for anything configured later.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// A cookie to send with `Set-Cookie`.
#[derive(Debug, Clone)]
pub struct SetCookie {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    /// Lifetime in seconds. `0` expires the cookie immediately, which is how a
    /// cookie is deleted: there is no "unset" — the server sends the same name
    /// back with a zero lifetime.
    pub max_age: Option<i64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<SameSite>,
}

impl SetCookie {
    /// A session cookie with defensible defaults.
    ///
    /// `HttpOnly` keeps it out of `document.cookie`, so a scripting bug in a
    /// served page cannot read the session id. `SameSite=Lax` stops it riding
    /// along on cross-site form posts, which is the CSRF case. `Secure` is
    /// **not** set by default: this server speaks plain HTTP, and a `Secure`
    /// cookie would simply never be sent back.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> SetCookie {
        SetCookie {
            name: name.into(),
            value: value.into(),
            path: Some("/".to_string()),
            max_age: None,
            http_only: true,
            secure: false,
            same_site: Some(SameSite::Lax),
        }
    }

    pub fn max_age(mut self, seconds: i64) -> SetCookie {
        self.max_age = Some(seconds);
        self
    }

    /// Narrow a cookie to a subtree. The session cookie uses the default `/`.
    #[allow(dead_code)]
    pub fn path(mut self, path: impl Into<String>) -> SetCookie {
        self.path = Some(path.into());
        self
    }

    #[allow(dead_code)]
    pub fn http_only(mut self, yes: bool) -> SetCookie {
        self.http_only = yes;
        self
    }

    #[allow(dead_code)]
    pub fn same_site(mut self, policy: SameSite) -> SetCookie {
        self.same_site = Some(policy);
        self
    }

    /// A cookie that deletes the one with the same name.
    pub fn expired(name: impl Into<String>) -> SetCookie {
        SetCookie::new(name, "").max_age(0)
    }

    /// True when the name and value can go on the wire unescaped.
    ///
    /// Rejecting is safer than escaping: a cookie value containing `;` or a
    /// newline could otherwise inject an attribute, or an entire extra header,
    /// into the response.
    pub fn is_valid(&self) -> bool {
        fn ok(text: &str) -> bool {
            !text.is_empty()
                && text.chars().all(|c| {
                    !c.is_control()
                        && !c.is_whitespace()
                        && !matches!(c, ';' | ',' | '=' | '"' | '\\')
                })
        }
        // An empty value is legitimate (that is how a cookie is deleted).
        ok(&self.name)
            && self.value.chars().all(|c| {
                !c.is_control() && !c.is_whitespace() && !matches!(c, ';' | ',' | '"' | '\\')
            })
    }
}

impl fmt::Display for SetCookie {
    /// Render the header value: `name=value; Path=/; Max-Age=1800; HttpOnly; SameSite=Lax`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.name, self.value)?;
        if let Some(path) = &self.path {
            write!(f, "; Path={path}")?;
        }
        if let Some(max_age) = self.max_age {
            write!(f, "; Max-Age={max_age}")?;
        }
        if self.http_only {
            write!(f, "; HttpOnly")?;
        }
        if self.secure {
            write!(f, "; Secure")?;
        }
        if let Some(policy) = self.same_site {
            write!(f, "; SameSite={}", policy.as_str())?;
        }
        Ok(())
    }
}

/// Parse a `Cookie:` header into its pairs.
///
/// Malformed pairs are skipped rather than failing the request: a stale or
/// mangled cookie from some other application on the same host must not make
/// the site unusable.
pub fn parse(header: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for part in header.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = match part.split_once('=') {
            Some(pair) => pair,
            // A bare token with no `=` is not a cookie.
            None => continue,
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let value = value.trim();
        // Values may be double-quoted; the quotes are not part of the value.
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        pairs.push((name.to_string(), value.to_string()));
    }
    pairs
}

/// The value of one cookie, or `None`.
pub fn get<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_a_single_cookie() {
        let pairs = parse("session=abc123");
        assert_eq!(pairs, vec![("session".to_string(), "abc123".to_string())]);
    }

    #[test]
    fn parses_several_cookies() {
        let pairs = parse("a=1; b=2; c=3");
        assert_eq!(pairs.len(), 3);
        assert_eq!(get(&pairs, "b"), Some("2"));
        assert_eq!(get(&pairs, "missing"), None);
    }

    #[test]
    fn tolerates_spacing_and_quotes() {
        let pairs = parse("  a=1 ;b = 2 ;  c=\"quoted\"  ");
        assert_eq!(get(&pairs, "a"), Some("1"));
        assert_eq!(get(&pairs, "b"), Some("2"));
        assert_eq!(get(&pairs, "c"), Some("quoted"));
    }

    /// A mangled cookie from another application on the same host must not
    /// make this site unusable.
    #[test]
    fn skips_malformed_pairs_instead_of_failing() {
        let pairs = parse("good=1; garbage; =novalue; ; other=2");
        assert_eq!(get(&pairs, "good"), Some("1"));
        assert_eq!(get(&pairs, "other"), Some("2"));
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn an_empty_value_is_preserved() {
        let pairs = parse("empty=; other=1");
        assert_eq!(get(&pairs, "empty"), Some(""));
    }

    #[test]
    fn builds_a_session_cookie_with_safe_defaults() {
        let cookie = SetCookie::new("sid", "deadbeef").max_age(1800);
        let rendered = cookie.to_string();
        assert!(rendered.starts_with("sid=deadbeef"));
        assert!(rendered.contains("; Path=/"));
        assert!(rendered.contains("; Max-Age=1800"));
        // Keeps the session id out of document.cookie.
        assert!(rendered.contains("; HttpOnly"));
        assert!(rendered.contains("; SameSite=Lax"));
        // This server speaks plain HTTP; a Secure cookie would never come back.
        assert!(!rendered.contains("; Secure"));
    }

    /// Deleting a cookie means re-sending it with a zero lifetime; there is no
    /// "unset".
    #[test]
    fn an_expired_cookie_deletes_the_original() {
        let rendered = SetCookie::expired("sid").to_string();
        assert!(rendered.starts_with("sid="));
        assert!(rendered.contains("; Max-Age=0"));
    }

    /// A value carrying `;` or a newline could inject an attribute, or a whole
    /// extra header, into the response.
    #[test]
    fn rejects_values_that_could_inject_headers() {
        assert!(SetCookie::new("sid", "abc123").is_valid());
        assert!(!SetCookie::new("sid", "abc; Path=/admin").is_valid());
        assert!(!SetCookie::new("sid", "abc\r\nX-Injected: 1").is_valid());
        assert!(!SetCookie::new("bad name", "v").is_valid());
        assert!(!SetCookie::new("", "v").is_valid());
        // An empty value is legitimate: that is a deletion.
        assert!(SetCookie::new("sid", "").is_valid());
    }

    /// What the server sends must be readable by the parser that receives it
    /// back.
    #[test]
    fn set_cookie_round_trips_through_the_request_parser() {
        let cookie = SetCookie::new("session", "0123abcd").max_age(60);
        let rendered = cookie.to_string();
        // A client sends back only the pair, never the attributes.
        let pair_only = rendered.split(';').next().unwrap();
        let pairs = parse(pair_only);
        assert_eq!(get(&pairs, "session"), Some("0123abcd"));
    }
}
