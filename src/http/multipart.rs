//! `multipart/form-data` parsing, for file uploads.
//!
//! # Why this must be byte-exact ([A-21])
//!
//! The audit uploads a file and downloads it again to check it was not
//! corrupted. Any off-by-one at a boundary, or any attempt to treat the body as
//! text, shows up immediately on a binary file (a PNG, a tarball) while passing
//! unnoticed on a text one. So parts are located by byte search and copied
//! verbatim; nothing here converts a body to a `String`.
//!
//! # Why filenames are sanitised
//!
//! The filename comes from the client and is used to build a path on disk. A
//! part claiming `../../etc/cron.d/evil` or `/etc/passwd` must not be able to
//! escape the upload directory, so [`sanitize_filename`] keeps only the final
//! component and rejects the dangerous shapes outright rather than trying to
//! rewrite them.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]
// Written and unit-tested here in Phase 3.3; the upload handler that calls it
// lands in TASKS.md 5.2.
#![allow(dead_code)]

/// One part of a multipart body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Part {
    /// The form field name, from `Content-Disposition: name="..."`.
    pub name: Option<String>,
    /// The client-supplied file name, exactly as sent (unsanitised).
    pub filename: Option<String>,
    pub content_type: Option<String>,
    /// The part's payload, byte for byte.
    pub data: Vec<u8>,
}

impl Part {
    /// The file name, reduced to something safe to join onto a directory.
    pub fn safe_filename(&self) -> Option<String> {
        self.filename.as_deref().and_then(sanitize_filename)
    }
}

/// Extract the `boundary=` parameter from a `Content-Type` header value.
pub fn boundary_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    if !lower.starts_with("multipart/") {
        return None;
    }
    for parameter in content_type.split(';').skip(1) {
        let parameter = parameter.trim();
        let (key, value) = parameter.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            if value.is_empty() {
                return None;
            }
            return Some(value.to_string());
        }
    }
    None
}

/// Split a `multipart/form-data` body into its parts.
///
/// Returns `Err(400)` for a body that does not follow the boundary structure.
pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, u16> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();

    // Find the first delimiter; anything before it is the preamble, which the
    // specification says to ignore.
    let mut pos = match find(body, &delimiter, 0) {
        Some(i) => i + delimiter.len(),
        None => return Err(400),
    };

    loop {
        // After a delimiter comes either "--" (end of body) or CRLF (a part).
        if body[pos..].starts_with(b"--") {
            return Ok(parts);
        }
        pos = match skip_crlf(body, pos) {
            Some(p) => p,
            None => return Err(400),
        };

        // Part headers, terminated by a blank line.
        let headers_end = match find(body, b"\r\n\r\n", pos) {
            Some(i) => i,
            None => return Err(400),
        };
        let header_text = std::str::from_utf8(&body[pos..headers_end]).map_err(|_| 400u16)?;
        let mut part = Part::default();
        for line in header_text.split("\r\n") {
            let (name, value) = match line.split_once(':') {
                Some(pair) => pair,
                None => continue,
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-disposition") {
                part.name = parameter_of(value, "name");
                part.filename = parameter_of(value, "filename");
            } else if name.eq_ignore_ascii_case("content-type") {
                part.content_type = Some(value.to_string());
            }
        }

        let data_start = headers_end + 4;
        // The next delimiter is preceded by a CRLF that belongs to the
        // delimiter, not to the data — getting this wrong appends two stray
        // bytes to every uploaded file.
        let next = match find(body, &delimiter, data_start) {
            Some(i) => i,
            None => return Err(400),
        };
        let data_end = next.saturating_sub(2);
        if data_end < data_start {
            return Err(400);
        }
        part.data = body[data_start..data_end].to_vec();
        parts.push(part);
        pos = next + delimiter.len();

        if pos > body.len() {
            return Err(400);
        }
    }
}

/// Reduce a client-supplied filename to a safe single path component.
///
/// Returns `None` when nothing safe remains, so the caller must decide what to
/// do rather than receiving a silently invented name.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    // Both separators: a Windows client may send a full `C:\path\file.txt`.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();

    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    // A leading dot would create a hidden file, and `..` anywhere is a
    // traversal attempt however it is spelled.
    if base.starts_with('.') || base.contains("..") {
        return None;
    }
    // Control characters and NUL must never reach a filesystem call.
    if base.chars().any(|c| c.is_control()) {
        return None;
    }
    if base.len() > 255 {
        return None;
    }
    Some(base.to_string())
}

/// Read a `key="value"` (or bare `key=value`) parameter out of a header value.
fn parameter_of(value: &str, key: &str) -> Option<String> {
    for parameter in value.split(';') {
        let parameter = parameter.trim();
        let (name, raw) = match parameter.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        if !name.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let raw = raw.trim();
        let unquoted = raw
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(raw);
        return Some(unquoted.to_string());
    }
    None
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn skip_crlf(body: &[u8], pos: usize) -> Option<usize> {
    if body[pos..].starts_with(b"\r\n") {
        Some(pos + 2)
    } else if body[pos..].starts_with(b"\n") {
        Some(pos + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn build(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, filename, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            let disposition = match filename {
                Some(f) => format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n"
                ),
                None => format!("Content-Disposition: form-data; name=\"{name}\"\r\n"),
            };
            body.extend_from_slice(disposition.as_bytes());
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    #[test]
    fn extracts_the_boundary_parameter() {
        assert_eq!(
            boundary_of("multipart/form-data; boundary=----abc123").as_deref(),
            Some("----abc123")
        );
        assert_eq!(
            boundary_of("multipart/form-data; boundary=\"quoted\"").as_deref(),
            Some("quoted")
        );
        assert_eq!(boundary_of("application/json"), None);
        assert_eq!(boundary_of("multipart/form-data"), None);
    }

    #[test]
    fn parses_a_single_file_part() {
        let body = build("XbY", &[("upload", Some("hello.txt"), b"file contents")]);
        let parts = parse(&body, "XbY").unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name.as_deref(), Some("upload"));
        assert_eq!(parts[0].filename.as_deref(), Some("hello.txt"));
        assert_eq!(parts[0].data, b"file contents");
    }

    #[test]
    fn parses_several_parts_including_plain_fields() {
        let body = build(
            "B",
            &[
                ("field", None, b"value"),
                ("file", Some("a.bin"), b"\x00\x01\x02"),
            ],
        );
        let parts = parse(&body, "B").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].data, b"value");
        assert!(parts[0].filename.is_none());
        assert_eq!(parts[1].data, vec![0u8, 1, 2]);
    }

    /// The test that catches the classic two-stray-bytes bug: the CRLF before a
    /// boundary belongs to the delimiter, not to the file.
    #[test]
    fn preserves_binary_content_exactly() {
        // Includes NULs, high bytes, CR and LF — everything a naive text-based
        // parser mangles.
        let payload: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0x0D, 0x0A, 0x2D, 0x2D,
        ];
        let body = build("bnd", &[("f", Some("x.png"), &payload)]);
        let parts = parse(&body, "bnd").unwrap();
        assert_eq!(parts[0].data, payload, "uploaded bytes must survive verbatim");
        assert_eq!(parts[0].data.len(), payload.len());
    }

    #[test]
    fn handles_an_empty_file_part() {
        let body = build("z", &[("f", Some("empty.txt"), b"")]);
        let parts = parse(&body, "z").unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].data.is_empty());
    }

    #[test]
    fn rejects_a_body_without_a_boundary() {
        assert_eq!(parse(b"not multipart at all", "bnd"), Err(400));
    }

    #[test]
    fn rejects_a_truncated_body() {
        let body = b"--bnd\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nvalue";
        assert_eq!(parse(body, "bnd"), Err(400));
    }

    #[test]
    fn sanitises_traversal_attempts_out_of_filenames() {
        assert_eq!(sanitize_filename("report.pdf").as_deref(), Some("report.pdf"));
        // Only the final component survives.
        assert_eq!(sanitize_filename("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_filename("/etc/shadow").as_deref(), Some("shadow"));
        assert_eq!(sanitize_filename(r"C:\Windows\evil.exe").as_deref(), Some("evil.exe"));
        // Nothing safe remains.
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("   "), None);
        // Hidden files and embedded traversal are refused outright.
        assert_eq!(sanitize_filename(".bashrc"), None);
        assert_eq!(sanitize_filename("a..b"), None);
        // Control characters must never reach a filesystem call.
        assert_eq!(sanitize_filename("bad\0name"), None);
        assert_eq!(sanitize_filename("bad\nname"), None);
    }

    #[test]
    fn reads_quoted_and_unquoted_parameters() {
        assert_eq!(
            parameter_of("form-data; name=\"file\"; filename=\"a b.txt\"", "filename").as_deref(),
            Some("a b.txt")
        );
        assert_eq!(parameter_of("form-data; name=plain", "name").as_deref(), Some("plain"));
        assert_eq!(parameter_of("form-data; name=\"x\"", "missing"), None);
    }
}
