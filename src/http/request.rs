//! Incremental HTTP/1.1 request parser.
//!
//! # Why incremental ([A-5] and [A-36] together)
//!
//! The event loop performs exactly one `read()` per readiness event, so a
//! request arrives in whatever fragments the network chose: a request line
//! split mid-word, a header split across two reads, a body dribbled a byte at a
//! time. The parser therefore never assumes a complete message is present. It
//! distinguishes three outcomes:
//!
//! * [`Progress::NeedMore`] — valid so far, keep what is buffered and wait.
//! * [`Progress::Complete`] — a whole request is available.
//! * [`Progress::Failed`] — malformed; answer that status and close.
//!
//! Keeping "incomplete" apart from "invalid" is what lets one-read-per-event
//! and correct parsing coexist. Treating a short read as an error would break
//! every client on a slow link.
//!
//! # What is rejected, and why
//!
//! Beyond obvious syntax errors, the parser refuses several things that are
//! syntactically parseable but dangerous:
//!
//! * Both `Content-Length` and `Transfer-Encoding` on one request (400) —
//!   the classic request-smuggling setup, where two servers disagree about
//!   where the body ends.
//! * Conflicting duplicate `Content-Length` values (400), for the same reason.
//! * Whitespace between a header name and its colon (400), which some parsers
//!   accept and others do not.
//! * A missing or repeated `Host` on HTTP/1.1 (400) — virtual hosting depends
//!   on it being unambiguous.
//! * An over-long header block (431) rather than buffering without bound.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use crate::config::Method;
use crate::http::chunked::{ChunkStatus, ChunkedDecoder};

/// Cap on the request line plus all headers.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Cap on the request line alone.
const MAX_REQUEST_LINE: usize = 4 * 1024;

/// Protocol version. Anything else answers 505.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

impl Version {
    pub fn as_str(self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
        }
    }
}

/// Header fields, stored in arrival order with lowercased names.
///
/// A `Vec` rather than a map because header names repeat legitimately
/// (`Set-Cookie`, `Cookie`), order is occasionally meaningful, and a request
/// has few enough headers that a linear scan beats hashing.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn push(&mut self, name: &str, value: &str) {
        self.entries.push((name.to_ascii_lowercase(), value.to_string()));
    }

    /// First value for a header. Names are matched case-insensitively, as
    /// RFC 9110 requires.
    pub fn get(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|(n, _)| *n == needle)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for a header, for the ones that legitimately repeat.
    pub fn get_all<'a>(&'a self, name: &str) -> Vec<&'a str> {
        let needle = name.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|(n, _)| *n == needle)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Is `token` present in a comma-separated list header such as
    /// `Connection: keep-alive, upgrade`?
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.get_all(name).iter().any(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    }

    // Used when building the CGI environment (7.2), which forwards every
    // request header as an HTTP_* variable.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A fully received request.
#[derive(Debug, Clone)]
pub struct Request {
    /// The method exactly as sent. Kept as text so an unknown but well-formed
    /// method can be answered 501 rather than 400 — it is a valid request for
    /// something this server does not implement.
    pub method: String,
    /// The request target as sent, e.g. `/files/a.txt?x=1`. Kept for the
    /// `Location` headers built by the redirect handler (5.x) and for logging.
    #[allow(dead_code)]
    pub target: String,
    /// Target with the query string removed, still percent-encoded.
    /// Decoding happens in `utils::path` (TASKS.md 4.3), because decoding
    /// before normalising `..` is how directory traversal gets through.
    pub path: String,
    /// Everything after the first `?`, without the `?`.
    pub query: String,
    pub version: Version,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    /// The method as a known verb, or `None` for one this server does not
    /// implement.
    pub fn method_enum(&self) -> Option<Method> {
        Method::parse(&self.method)
    }

    /// `Host` with any `:port` removed, lowercased — the key for virtual host
    /// selection ([A-11]).
    pub fn host(&self) -> Option<String> {
        let raw = self.headers.get("host")?;
        // An IPv6 literal is bracketed: [::1]:8080.
        let without_port = if let Some(rest) = raw.strip_prefix('[') {
            match rest.find(']') {
                Some(end) => &raw[..end + 2],
                None => raw,
            }
        } else {
            match raw.rfind(':') {
                Some(i) => &raw[..i],
                None => raw,
            }
        };
        Some(without_port.trim().to_ascii_lowercase())
    }

    /// Should the connection stay open after this response?
    ///
    /// HTTP/1.1 defaults to keep-alive, HTTP/1.0 to close; either can be
    /// overridden by the `Connection` header.
    pub fn wants_keep_alive(&self) -> bool {
        if self.headers.has_token("connection", "close") {
            return false;
        }
        match self.version {
            Version::Http11 => true,
            Version::Http10 => self.headers.has_token("connection", "keep-alive"),
        }
    }

    /// Does the client expect a `100 Continue` before sending its body?
    pub fn expects_continue(&self) -> bool {
        self.headers
            .get("expect")
            .map(|v| v.trim().eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
    }
}

/// How the body's length is determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    None,
    Length(usize),
    Chunked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Headers,
    Body,
    Done,
}

/// What a `feed` call concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Valid so far; more bytes needed.
    NeedMore,
    /// A complete request is ready; call [`RequestParser::take`].
    Complete,
    /// Malformed. The `u16` is the status to answer with.
    Failed(u16),
}

pub struct RequestParser {
    phase: Phase,
    /// Bytes received but not yet consumed.
    buf: Vec<u8>,
    head: Option<Request>,
    body_kind: BodyKind,
    decoder: ChunkedDecoder,
    max_body: usize,
    body: Vec<u8>,
}

impl RequestParser {
    pub fn new(max_body: usize) -> RequestParser {
        RequestParser {
            phase: Phase::Headers,
            buf: Vec::new(),
            head: None,
            body_kind: BodyKind::None,
            decoder: ChunkedDecoder::new(),
            max_body,
            body: Vec::new(),
        }
    }

    /// Narrow the body limit once the virtual host is known.
    ///
    /// The listener only knows the largest limit among the servers sharing it;
    /// the selected server's own `client_max_body_size` is stricter and is
    /// applied as soon as the `Host` header has been read (TASKS.md 4.1).
    // Called by the router once the Host header selects a virtual host (4.1).
    #[allow(dead_code)]
    pub fn set_max_body(&mut self, limit: usize) {
        self.max_body = limit;
    }

    /// True once the header block has been parsed, even if the body is still
    /// arriving — the point at which the virtual host can be selected.
    pub fn headers_ready(&self) -> bool {
        self.head.is_some()
    }

    pub fn head(&self) -> Option<&Request> {
        self.head.as_ref()
    }

    /// Feed newly read bytes. Safe to call with an empty slice to re-drive the
    /// state machine over what is already buffered.
    pub fn feed(&mut self, input: &[u8]) -> Progress {
        self.buf.extend_from_slice(input);
        loop {
            match self.phase {
                Phase::Done => return Progress::Complete,

                Phase::Headers => {
                    let end = match find_header_end(&self.buf) {
                        Some(end) => end,
                        None => {
                            // Nothing wrong yet, just incomplete — unless the
                            // client is trying to exhaust memory with headers.
                            if self.buf.len() > MAX_HEADER_BYTES {
                                return Progress::Failed(431);
                            }
                            return Progress::NeedMore;
                        }
                    };
                    if end > MAX_HEADER_BYTES {
                        return Progress::Failed(431);
                    }
                    let block: Vec<u8> = self.buf.drain(..end).collect();
                    let request = match parse_head(&block) {
                        Ok(r) => r,
                        Err(status) => return Progress::Failed(status),
                    };
                    self.body_kind = match body_kind_of(&request) {
                        Ok(kind) => kind,
                        Err(status) => return Progress::Failed(status),
                    };
                    // A declared length can be refused before a single body
                    // byte is buffered ([A-13]).
                    if let BodyKind::Length(n) = self.body_kind {
                        if n > self.max_body {
                            return Progress::Failed(413);
                        }
                    }
                    self.head = Some(request);
                    self.phase = if self.body_kind == BodyKind::None {
                        Phase::Done
                    } else {
                        Phase::Body
                    };
                }

                Phase::Body => match self.body_kind {
                    BodyKind::None => self.phase = Phase::Done,

                    BodyKind::Length(total) => {
                        let needed = total - self.body.len();
                        let take = needed.min(self.buf.len());
                        self.body.extend(self.buf.drain(..take));
                        if self.body.len() < total {
                            return Progress::NeedMore;
                        }
                        self.phase = Phase::Done;
                    }

                    BodyKind::Chunked => {
                        let input = std::mem::take(&mut self.buf);
                        let (consumed, status) =
                            self.decoder.feed(&input, &mut self.body, self.max_body);
                        // Whatever the decoder did not consume stays buffered.
                        self.buf.extend_from_slice(&input[consumed..]);
                        match status {
                            ChunkStatus::NeedMore => return Progress::NeedMore,
                            ChunkStatus::Done => self.phase = Phase::Done,
                            ChunkStatus::Malformed => return Progress::Failed(400),
                            ChunkStatus::TooLarge => return Progress::Failed(413),
                        }
                    }
                },
            }
        }
    }

    /// Take the finished request, leaving the parser ready for the next one on
    /// this connection (keep-alive) along with any pipelined bytes already
    /// received.
    pub fn take(&mut self) -> Option<Request> {
        if self.phase != Phase::Done {
            return None;
        }
        let mut request = self.head.take()?;
        request.body = std::mem::take(&mut self.body);
        self.phase = Phase::Headers;
        self.body_kind = BodyKind::None;
        self.decoder = ChunkedDecoder::new();
        Some(request)
    }

    /// Bytes buffered but not yet part of a request — a pipelined follow-up.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Index just past the `\r\n\r\n` (or `\n\n`) ending the header block.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(i + 4);
    }
    // Tolerated for hand-typed requests over netcat, which the audit uses.
    buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
}

/// Parse the request line and header fields.
fn parse_head(block: &[u8]) -> Result<Request, u16> {
    let text = std::str::from_utf8(block).map_err(|_| 400u16)?;
    // RFC 9112 allows a server to ignore empty lines before the request line.
    let text = text.trim_start_matches("\r\n").trim_start_matches('\n');

    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));

    let request_line = lines.next().ok_or(400u16)?;
    if request_line.len() > MAX_REQUEST_LINE {
        return Err(414);
    }
    let (method, target, version) = parse_request_line(request_line)?;

    let mut headers = Headers::new_empty();
    for line in lines {
        if line.is_empty() {
            break;
        }
        // Obsolete line folding: a continuation line starting with whitespace.
        // RFC 9112 says a server may reject it, and accepting it is a known
        // source of parser disagreement.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(400);
        }
        let colon = line.find(':').ok_or(400u16)?;
        let name = &line[..colon];
        // "No whitespace is allowed between the field name and colon."
        if name.is_empty() || name.ends_with(' ') || name.ends_with('\t') {
            return Err(400);
        }
        if !name.bytes().all(is_token_byte) {
            return Err(400);
        }
        let value = line[colon + 1..].trim_matches([' ', '\t']);
        headers.push(name, value);
    }

    let (path, query) = split_target(&target);

    let request = Request {
        method,
        target,
        path,
        query,
        version,
        headers,
        body: Vec::new(),
    };

    // Virtual hosting depends on Host being present and unambiguous.
    let hosts = request.headers.get_all("host");
    if request.version == Version::Http11 && hosts.is_empty() {
        return Err(400);
    }
    if hosts.len() > 1 {
        return Err(400);
    }

    Ok(request)
}

fn parse_request_line(line: &str) -> Result<(String, String, Version), u16> {
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(400);
    }
    if method.is_empty() || target.is_empty() || version.is_empty() {
        return Err(400);
    }
    if !method.bytes().all(is_token_byte) {
        return Err(400);
    }
    let version = match version {
        "HTTP/1.1" => Version::Http11,
        "HTTP/1.0" => Version::Http10,
        // Well-formed but unsupported is 505, not 400.
        v if v.starts_with("HTTP/") => return Err(505),
        _ => return Err(400),
    };
    Ok((method.to_string(), target.to_string(), version))
}

/// Split a request target into path and query.
///
/// Accepts origin-form (`/a/b?c`) and absolute-form
/// (`http://host/a/b?c`, which a client may use); `*` is kept as-is for
/// `OPTIONS *`.
fn split_target(target: &str) -> (String, String) {
    let without_scheme = if let Some(rest) = target.strip_prefix("http://") {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    } else if let Some(rest) = target.strip_prefix("https://") {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    } else {
        target
    };

    match without_scheme.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (without_scheme.to_string(), String::new()),
    }
}

/// Decide how the body length is framed, rejecting the ambiguous combinations.
fn body_kind_of(request: &Request) -> Result<BodyKind, u16> {
    let transfer_encoding = request.headers.get_all("transfer-encoding");
    let content_length = request.headers.get_all("content-length");

    if !transfer_encoding.is_empty() {
        if !content_length.is_empty() {
            // Two different framings: the classic request-smuggling setup.
            return Err(400);
        }
        let is_chunked = transfer_encoding
            .iter()
            .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("chunked")));
        if !is_chunked {
            // gzip, compress and friends are not implemented.
            return Err(501);
        }
        return Ok(BodyKind::Chunked);
    }

    if content_length.is_empty() {
        return Ok(BodyKind::None);
    }

    let mut parsed: Option<usize> = None;
    for value in content_length {
        let n: usize = value.trim().parse().map_err(|_| 400u16)?;
        match parsed {
            // Repeated but identical is tolerated; conflicting is not.
            Some(previous) if previous != n => return Err(400),
            _ => parsed = Some(n),
        }
    }
    match parsed {
        Some(0) => Ok(BodyKind::None),
        Some(n) => Ok(BodyKind::Length(n)),
        None => Ok(BodyKind::None),
    }
}

/// Characters allowed in a token (method names, header names) per RFC 9110.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_'
                | b'`' | b'|' | b'~'
        )
}

impl Headers {
    fn new_empty() -> Headers {
        Headers { entries: Vec::new() }
    }
}

// ---------------------------------------------------------------------------
// Tests — audit question [A-36] (HTTP request parsing) and the subject's
// "HTTP Request Parsing" unit-test requirement.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Feed a whole request in one go.
    fn parse_all(bytes: &[u8], max_body: usize) -> (Progress, Option<Request>) {
        let mut parser = RequestParser::new(max_body);
        let progress = parser.feed(bytes);
        let request = parser.take();
        (progress, request)
    }

    // --- request line and headers -------------------------------------------

    #[test]
    fn parses_a_simple_get() {
        let (progress, request) = parse_all(b"GET /index.html HTTP/1.1\r\nHost: example.com\r\n\r\n", 1024);
        assert_eq!(progress, Progress::Complete);
        let request = request.unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.method_enum(), Some(Method::Get));
        assert_eq!(request.path, "/index.html");
        assert_eq!(request.query, "");
        assert_eq!(request.version, Version::Http11);
        assert_eq!(request.headers.get("host"), Some("example.com"));
        assert!(request.body.is_empty());
    }

    #[test]
    fn splits_the_query_string_from_the_path() {
        let (_, request) = parse_all(b"GET /search?q=rust&n=10 HTTP/1.1\r\nHost: x\r\n\r\n", 1024);
        let request = request.unwrap();
        assert_eq!(request.path, "/search");
        assert_eq!(request.query, "q=rust&n=10");
        assert_eq!(request.target, "/search?q=rust&n=10");
    }

    #[test]
    fn accepts_absolute_form_targets() {
        let (_, request) = parse_all(b"GET http://example.com/a/b?c=1 HTTP/1.1\r\nHost: example.com\r\n\r\n", 1024);
        let request = request.unwrap();
        assert_eq!(request.path, "/a/b");
        assert_eq!(request.query, "c=1");
    }

    /// Header names are case-insensitive; values are not.
    #[test]
    fn header_lookup_ignores_name_case() {
        let (_, request) = parse_all(b"GET / HTTP/1.1\r\nHost: x\r\nX-Custom-Value: KeepCase\r\n\r\n", 1024);
        let request = request.unwrap();
        assert_eq!(request.headers.get("x-custom-value"), Some("KeepCase"));
        assert_eq!(request.headers.get("X-CUSTOM-VALUE"), Some("KeepCase"));
    }

    #[test]
    fn parses_comma_separated_list_headers() {
        let (_, request) = parse_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, upgrade\r\n\r\n", 1024);
        let request = request.unwrap();
        assert!(request.headers.has_token("connection", "keep-alive"));
        assert!(request.headers.has_token("connection", "UPGRADE"));
        assert!(!request.headers.has_token("connection", "close"));
    }

    #[test]
    fn strips_the_port_from_the_host_header() {
        let (_, request) = parse_all(b"GET / HTTP/1.1\r\nHost: Test.COM:8080\r\n\r\n", 1024);
        assert_eq!(request.unwrap().host().as_deref(), Some("test.com"));
        let (_, request) = parse_all(b"GET / HTTP/1.1\r\nHost: example.org\r\n\r\n", 1024);
        assert_eq!(request.unwrap().host().as_deref(), Some("example.org"));
    }

    #[test]
    fn parses_post_with_a_content_length_body() {
        let (progress, request) =
            parse_all(b"POST /submit HTTP/1.1\r\nHost: x\r\nContent-Length: 11\r\n\r\nhello world", 1024);
        assert_eq!(progress, Progress::Complete);
        let request = request.unwrap();
        assert_eq!(request.method_enum(), Some(Method::Post));
        assert_eq!(request.body, b"hello world");
    }

    #[test]
    fn parses_delete() {
        let (progress, request) = parse_all(b"DELETE /files/a.txt HTTP/1.1\r\nHost: x\r\n\r\n", 1024);
        assert_eq!(progress, Progress::Complete);
        assert_eq!(request.unwrap().method_enum(), Some(Method::Delete));
    }

    #[test]
    fn parses_a_chunked_body() {
        let (progress, request) = parse_all(
            b"POST /u HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            1024,
        );
        assert_eq!(progress, Progress::Complete);
        assert_eq!(request.unwrap().body, b"hello world");
    }

    // --- incremental delivery ------------------------------------------------

    /// The property the whole design rests on: because the loop does one read
    /// per event, a request may arrive one byte at a time, and must parse
    /// identically.
    #[test]
    fn parses_a_request_delivered_one_byte_at_a_time() {
        let wire = b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nabcde";
        let mut parser = RequestParser::new(1024);
        for (i, byte) in wire.iter().enumerate() {
            let progress = parser.feed(&[*byte]);
            if i + 1 < wire.len() {
                assert_eq!(progress, Progress::NeedMore, "byte {i} ended the request early");
            } else {
                assert_eq!(progress, Progress::Complete);
            }
        }
        let request = parser.take().unwrap();
        assert_eq!(request.body, b"abcde");
    }

    #[test]
    fn parses_a_chunked_body_delivered_one_byte_at_a_time() {
        let wire = b"POST /x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        let mut parser = RequestParser::new(1024);
        let mut progress = Progress::NeedMore;
        for byte in wire.iter() {
            progress = parser.feed(&[*byte]);
        }
        assert_eq!(progress, Progress::Complete);
        assert_eq!(parser.take().unwrap().body, b"abc");
    }

    /// A header block that has not arrived yet is incomplete, never invalid.
    #[test]
    fn an_unterminated_header_block_is_incomplete_not_invalid() {
        let mut parser = RequestParser::new(1024);
        assert_eq!(parser.feed(b"GET / HTTP/1.1\r\nHost: x\r\n"), Progress::NeedMore);
        assert!(parser.take().is_none());
        assert_eq!(parser.feed(b"\r\n"), Progress::Complete);
    }

    /// Two requests in one read: the second must survive `take()`.
    #[test]
    fn keeps_pipelined_bytes_for_the_next_request() {
        let mut parser = RequestParser::new(1024);
        let progress = parser.feed(b"GET /first HTTP/1.1\r\nHost: x\r\n\r\nGET /second HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(progress, Progress::Complete);
        assert_eq!(parser.take().unwrap().path, "/first");
        assert!(parser.buffered() > 0, "the second request must still be buffered");
        assert_eq!(parser.feed(&[]), Progress::Complete);
        assert_eq!(parser.take().unwrap().path, "/second");
    }

    // --- status code generation ----------------------------------------------

    #[test]
    fn malformed_request_lines_are_bad_requests() {
        for wire in [
            &b"GARBAGE\r\n\r\n"[..],
            b"GET\r\n\r\n",
            b"GET /only-two-parts\r\n\r\n",
            b"GET / HTTP/1.1 extra\r\n\r\n",
            b"GE T / HTTP/1.1\r\n\r\n",
            b"G(ET) / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(parse_all(wire, 1024).0, Progress::Failed(400), "{:?}", String::from_utf8_lossy(wire));
        }
    }

    #[test]
    fn an_unsupported_version_is_505() {
        assert_eq!(parse_all(b"GET / HTTP/2.0\r\nHost: x\r\n\r\n", 1024).0, Progress::Failed(505));
        assert_eq!(parse_all(b"GET / HTTP/9.9\r\nHost: x\r\n\r\n", 1024).0, Progress::Failed(505));
        // Not a version at all is a malformed request line, not 505.
        assert_eq!(parse_all(b"GET / SPDY\r\nHost: x\r\n\r\n", 1024).0, Progress::Failed(400));
    }

    #[test]
    fn http_11_without_host_is_rejected() {
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\n\r\n", 1024).0, Progress::Failed(400));
        // HTTP/1.0 predates virtual hosting and needs no Host.
        assert_eq!(parse_all(b"GET / HTTP/1.0\r\n\r\n", 1024).0, Progress::Complete);
    }

    #[test]
    fn a_repeated_host_header_is_rejected() {
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", 1024).0, Progress::Failed(400));
    }

    #[test]
    fn malformed_headers_are_rejected() {
        // Whitespace before the colon.
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\nHost : x\r\n\r\n", 1024).0, Progress::Failed(400));
        // No colon at all.
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\nHost x\r\n\r\n", 1024).0, Progress::Failed(400));
        // Obsolete line folding.
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\nHost: x\r\n  continued\r\n\r\n", 1024).0, Progress::Failed(400));
        // Empty header name.
        assert_eq!(parse_all(b"GET / HTTP/1.1\r\n: value\r\n\r\n", 1024).0, Progress::Failed(400));
    }

    /// Two different ways of framing the body is the classic request-smuggling
    /// setup, and must be refused rather than guessed at.
    #[test]
    fn content_length_with_transfer_encoding_is_rejected() {
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello";
        assert_eq!(parse_all(wire, 1024).0, Progress::Failed(400));
    }

    #[test]
    fn conflicting_content_lengths_are_rejected() {
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello";
        assert_eq!(parse_all(wire, 1024).0, Progress::Failed(400));
        // Repeated but identical is harmless.
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_all(wire, 1024).0, Progress::Complete);
    }

    #[test]
    fn a_non_numeric_content_length_is_rejected() {
        assert_eq!(parse_all(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: abc\r\n\r\n", 1024).0, Progress::Failed(400));
    }

    #[test]
    fn an_unsupported_transfer_encoding_is_501() {
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert_eq!(parse_all(wire, 1024).0, Progress::Failed(501));
    }

    /// [A-13]: a declared length over the limit is refused before a single body
    /// byte is buffered.
    #[test]
    fn an_oversized_declared_body_is_413_before_reading_it() {
        let mut parser = RequestParser::new(10);
        let progress = parser.feed(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5000\r\n\r\n");
        assert_eq!(progress, Progress::Failed(413));
    }

    /// A chunked body cannot be checked in advance, so the limit applies as it
    /// arrives.
    #[test]
    fn an_oversized_chunked_body_is_413_while_arriving() {
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n20\r\n00000000000000000000000000000000\r\n0\r\n\r\n";
        assert_eq!(parse_all(wire, 8).0, Progress::Failed(413));
    }

    #[test]
    fn a_body_within_the_limit_is_accepted() {
        let wire = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_all(wire, 5).0, Progress::Complete);
    }

    #[test]
    fn an_oversized_header_block_is_431() {
        let mut wire = b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec();
        while wire.len() < MAX_HEADER_BYTES + 100 {
            wire.extend_from_slice(b"X-Padding: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        let mut parser = RequestParser::new(1024);
        assert_eq!(parser.feed(&wire), Progress::Failed(431));
    }

    #[test]
    fn an_over_long_request_line_is_414() {
        let mut wire = b"GET /".to_vec();
        wire.extend(std::iter::repeat(b'a').take(5000));
        wire.extend_from_slice(b" HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(parse_all(&wire, 1024).0, Progress::Failed(414));
    }

    // --- keep-alive and expectations -----------------------------------------

    #[test]
    fn keep_alive_defaults_follow_the_version() {
        let (_, r) = parse_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", 1024);
        assert!(r.unwrap().wants_keep_alive(), "HTTP/1.1 defaults to keep-alive");

        let (_, r) = parse_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", 1024);
        assert!(!r.unwrap().wants_keep_alive());

        let (_, r) = parse_all(b"GET / HTTP/1.0\r\n\r\n", 1024);
        assert!(!r.unwrap().wants_keep_alive(), "HTTP/1.0 defaults to close");

        let (_, r) = parse_all(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n", 1024);
        assert!(r.unwrap().wants_keep_alive());
    }

    #[test]
    fn detects_the_100_continue_expectation() {
        let mut parser = RequestParser::new(1024);
        parser.feed(b"POST / HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n");
        assert!(parser.headers_ready());
        assert!(parser.head().unwrap().expects_continue());
    }

    /// The virtual host's stricter limit can be applied once the headers are in.
    #[test]
    fn the_body_limit_can_be_narrowed_after_the_headers() {
        let mut parser = RequestParser::new(1_000_000);
        let progress = parser.feed(b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert_eq!(progress, Progress::NeedMore);
        assert!(parser.headers_ready());
        parser.set_max_body(4);
        assert_eq!(parser.feed(b"8\r\n01234567\r\n"), Progress::Failed(413));
    }

    #[test]
    fn tolerates_leading_blank_lines_before_the_request_line() {
        let (progress, request) = parse_all(b"\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n", 1024);
        assert_eq!(progress, Progress::Complete);
        assert_eq!(request.unwrap().path, "/");
    }

    /// Requests typed by hand over netcat use bare LF, which the audit does.
    #[test]
    fn tolerates_bare_lf_line_endings() {
        let (progress, request) = parse_all(b"GET / HTTP/1.1\nHost: x\n\n", 1024);
        assert_eq!(progress, Progress::Complete);
        assert_eq!(request.unwrap().path, "/");
    }
}
