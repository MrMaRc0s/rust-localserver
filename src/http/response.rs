//! Response construction and serialization.
//!
//! # Why the headers matter ([A-24])
//!
//! The audit checks response headers in the browser's network panel, and a
//! browser is far less forgiving than `curl`: a missing or wrong
//! `Content-Length` makes it hang waiting for bytes that never come, which
//! reads as "the server is broken". So every response gets
//! `Content-Length` (or `Transfer-Encoding: chunked`), a `Date`, a `Server`
//! and an explicit `Connection`.
//!
//! `Date` is formatted here rather than pulled from a crate: RFC 9110 requires
//! the IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`) in the C locale, and
//! deriving it from a Unix timestamp is a dozen lines of arithmetic.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::time::{SystemTime, UNIX_EPOCH};

/// The reason phrase for a status code.
///
/// Only the codes this server actually emits are listed; anything else falls
/// back to a generic phrase for its class rather than panicking.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        s if (100..200).contains(&s) => "Informational",
        s if (200..300).contains(&s) => "Success",
        s if (300..400).contains(&s) => "Redirection",
        s if (400..500).contains(&s) => "Client Error",
        _ => "Server Error",
    }
}

/// A response under construction.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Suppress the body when serializing, for `HEAD`. The `Content-Length`
    /// still describes the body that a `GET` would have returned, as RFC 9110
    /// requires.
    head_only: bool,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response { status, headers: Vec::new(), body: Vec::new(), head_only: false }
    }

    /// A response with a body and an explicit content type.
    pub fn with_body(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response::new(status)
            .header("Content-Type", content_type)
            .body(body)
    }

    /// A short `text/html` response, used for error pages and redirects.
    pub fn html(status: u16, body: impl Into<String>) -> Response {
        Response::with_body(status, "text/html; charset=utf-8", body.into().into_bytes())
    }

    pub fn header(mut self, name: &str, value: impl AsRef<str>) -> Response {
        self.headers.push((name.to_string(), value.as_ref().to_string()));
        self
    }

    /// Add a header only if that name is not already set.
    // Used by the handlers in Phase 5 to fill in Content-Type only when a CGI
    // script has not already set one.
    #[allow(dead_code)]
    pub fn default_header(mut self, name: &str, value: impl AsRef<str>) -> Response {
        if !self.has_header(name) {
            self.headers.push((name.to_string(), value.as_ref().to_string()));
        }
        self
    }

    /// Overwrite a header that was already set.
    pub fn replace_header(mut self, name: &str, value: impl AsRef<str>) -> Response {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.as_ref().to_string()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Response {
        self.body = body;
        self
    }

    pub fn head_only(mut self, yes: bool) -> Response {
        self.head_only = yes;
        self
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    #[allow(dead_code)]
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Serialize to the bytes that go on the wire.
    ///
    /// `keep_alive` decides the `Connection` header, so the caller (which knows
    /// whether the request asked to keep the connection and whether the server
    /// is willing) makes that decision rather than this builder.
    pub fn to_bytes(&self, keep_alive: bool) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            self.status,
            reason_phrase(self.status)
        );

        // 204 and 304 are defined to carry no body, and RFC 9110 says not to
        // send a Content-Length for them: some clients treat one as a promise
        // of bytes that never arrive and hold the connection open.
        let bodyless = matches!(self.status, 204 | 304);
        // Content-Length is mandatory unless the body is framed some other way.
        let framed = self.has_header("transfer-encoding");
        if !bodyless && !framed && !self.has_header("content-length") {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        if !self.has_header("date") {
            head.push_str(&format!("Date: {}\r\n", http_date(SystemTime::now())));
        }
        if !self.has_header("server") {
            head.push_str("Server: localserver\r\n");
        }
        if !self.has_header("connection") {
            head.push_str(if keep_alive {
                "Connection: keep-alive\r\n"
            } else {
                "Connection: close\r\n"
            });
        }
        if !self.has_header("access-control-allow-origin") {
            head.push_str("Access-Control-Allow-Origin: *\r\n");
        }
        if !self.has_header("access-control-allow-methods") {
            head.push_str("Access-Control-Allow-Methods: GET, POST, DELETE, HEAD, OPTIONS\r\n");
        }
        if !self.has_header("access-control-allow-headers") {
            head.push_str("Access-Control-Allow-Headers: *\r\n");
        }
        if !self.has_header("access-control-expose-headers") {
            head.push_str("Access-Control-Expose-Headers: *\r\n");
        }
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        let mut out = head.into_bytes();
        if !self.head_only {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// Format a timestamp as an RFC 9110 IMF-fixdate, e.g.
/// `Sun, 06 Nov 1994 08:49:37 GMT`. Always GMT, always English abbreviations —
/// the format is fixed by the spec and is not locale-dependent.
pub fn http_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    );

    // 1970-01-01 was a Thursday, index 4 in a Sunday-first week.
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    let (year, month, day) = civil_from_days(days);

    format!(
        "{weekday}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} GMT",
        MONTHS[(month - 1) as usize]
    )
}

/// Convert days since 1970-01-01 to a civil (year, month, day).
///
/// Howard Hinnant's algorithm: shift the epoch to 0000-03-01 so leap days land
/// at the end of the year, which removes every February special case.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153; // [0, 11], March-based
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(http_date(at(0)), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    /// The example date from the HTTP specification itself.
    #[test]
    fn formats_the_rfc_example() {
        assert_eq!(http_date(at(784_111_777)), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn handles_leap_days_and_century_rules() {
        // 2000 was a leap year (divisible by 400).
        assert_eq!(http_date(at(951_782_400)), "Tue, 29 Feb 2000 00:00:00 GMT");
        // 2024-02-29, an ordinary leap year.
        assert_eq!(http_date(at(1_709_164_800)), "Thu, 29 Feb 2024 00:00:00 GMT");
        // A year boundary.
        assert_eq!(http_date(at(1_735_689_599)), "Tue, 31 Dec 2024 23:59:59 GMT");
        assert_eq!(http_date(at(1_735_689_600)), "Wed, 01 Jan 2025 00:00:00 GMT");
    }

    #[test]
    fn serializes_a_minimal_response() {
        let bytes = Response::with_body(200, "text/plain", b"hi".to_vec()).to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.contains("Server: localserver\r\n"));
        assert!(text.contains("Date: "));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn connection_header_follows_the_caller() {
        let response = Response::new(200);
        assert!(String::from_utf8_lossy(&response.to_bytes(false)).contains("Connection: close"));
        assert!(
            String::from_utf8_lossy(&response.to_bytes(true)).contains("Connection: keep-alive")
        );
    }

    /// A HEAD response carries the length the body would have had, but no body.
    #[test]
    fn head_responses_omit_the_body_but_keep_the_length() {
        let bytes = Response::with_body(200, "text/plain", b"0123456789".to_vec())
            .head_only(true)
            .to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Content-Length: 10\r\n"));
        assert!(text.ends_with("\r\n\r\n"), "no body should follow the headers");
    }

    #[test]
    fn explicit_headers_are_not_duplicated_by_defaults() {
        let bytes = Response::new(204)
            .header("Connection", "close")
            .header("Content-Length", "0")
            .to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.matches("Connection:").count(), 1);
        assert_eq!(text.matches("Content-Length:").count(), 1);
        assert!(text.contains("Connection: close"));
    }

    /// A chunked response must not also carry a Content-Length.
    #[test]
    fn a_204_carries_neither_body_nor_length() {
        let bytes = Response::new(204).to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(!text.contains("Content-Length"), "204 must not announce a length");
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn chunked_responses_get_no_content_length() {
        let bytes = Response::new(200)
            .header("Transfer-Encoding", "chunked")
            .to_bytes(true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("Content-Length"));
        assert!(text.contains("Transfer-Encoding: chunked"));
    }

    #[test]
    fn unknown_statuses_fall_back_to_a_class_phrase() {
        assert_eq!(reason_phrase(404), "Not Found");
        assert_eq!(reason_phrase(499), "Client Error");
        assert_eq!(reason_phrase(599), "Server Error");
    }
}
