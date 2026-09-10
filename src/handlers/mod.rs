//! Request handlers: static files, uploads, deletion and directory listings.
//!
//! # Streaming, not slurping ([A-17], [A-33])
//!
//! A file response is **not** read into memory. [`file`] opens the file, takes
//! `Content-Length` from its metadata, and hands back a [`FileBody`] that the
//! connection pulls from 64 KB at a time, one chunk per writable event. Reading
//! a 500 MB download into a `Vec` first would spike memory by 500 MB per
//! concurrent client and stall the single thread while it happened.
//!
//! Regular-file I/O is the one thing here that is not driven by epoll, and that
//! is not a shortcut: `epoll` cannot watch a regular file — it reports one as
//! always ready — so there is nothing to wait for. Keeping each read to one
//! bounded chunk per writable event is what keeps a large download from
//! monopolising the loop, which is the same reasoning as one socket read per
//! event.
//!
//! # Disk writes
//!
//! Upload and delete do block for the duration of one syscall. The write is
//! bounded by `client_max_body_size`, which is exactly why that limit is
//! validated at startup and enforced while the body arrives.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod session_page;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::http::multipart;
use crate::http::request::Request;
use crate::http::response::{http_date, Response};
use crate::utils::mime;

/// Bytes pulled from a file per writable event.
pub const STREAM_CHUNK: usize = 64 * 1024;

/// A response body still on disk, streamed as the socket accepts it.
#[derive(Debug)]
pub struct FileBody {
    pub file: File,
    /// Bytes still to send. Taken from the metadata read when the response was
    /// built, so `Content-Length` and what is actually sent agree even if the
    /// file changes underneath.
    pub remaining: u64,
}

/// A response, plus an optional body that is streamed rather than buffered.
#[derive(Debug)]
pub struct Reply {
    pub response: Response,
    pub body: Option<FileBody>,
}

impl Reply {
    pub fn immediate(response: Response) -> Reply {
        Reply { response, body: None }
    }
}

/// Translate a filesystem error into the status the client should see.
///
/// Deliberately coarse: the client learns that it may not have the file, not
/// why, and never sees a raw OS message.
fn status_for(error: &io::Error) -> u16 {
    match error.kind() {
        io::ErrorKind::NotFound => 404,
        io::ErrorKind::PermissionDenied => 403,
        _ => 500,
    }
}

/// Serve a regular file.
///
/// `head_only` suppresses the body but keeps `Content-Length`, as RFC 9110
/// requires: a `HEAD` must report what a `GET` would have returned.
pub fn file(path: &Path, head_only: bool) -> Result<Reply, u16> {
    let metadata = fs::metadata(path).map_err(|e| status_for(&e))?;
    if !metadata.is_file() {
        return Err(403);
    }
    let length = metadata.len();

    let mut response = Response::new(200)
        .header("Content-Type", mime::for_path(path))
        .header("Content-Length", length.to_string());
    if let Ok(modified) = metadata.modified() {
        response = response.header("Last-Modified", http_date(modified));
    }

    if head_only {
        return Ok(Reply::immediate(response.head_only(true)));
    }

    // Opened after the metadata read, so a file that vanished in between is
    // reported rather than sent as a truncated 200.
    let mut handle = File::open(path).map_err(|e| status_for(&e))?;

    // A file that fits in one chunk goes out with its headers in a single
    // write. Streaming it instead would cost three syscalls and two epoll
    // re-arms to move a few kilobytes, which is most of a static site's
    // traffic. The memory bound is unchanged: never more than one chunk.
    if length <= STREAM_CHUNK as u64 {
        let mut body = Vec::with_capacity(length as usize);
        handle.read_to_end(&mut body).map_err(|e| status_for(&e))?;
        // The file shrank between the metadata read and now; Content-Length
        // must describe what is actually sent.
        if body.len() as u64 != length {
            response = response.replace_header("Content-Length", body.len().to_string());
        }
        return Ok(Reply::immediate(response.body(body)));
    }

    Ok(Reply {
        response,
        body: Some(FileBody { file: handle, remaining: length }),
    })
}

/// Delete a file.
///
/// 204 rather than 200: there is nothing meaningful to return, and a body on a
/// 204 is forbidden.
pub fn delete(path: &Path) -> Result<Reply, u16> {
    let metadata = fs::metadata(path).map_err(|e| status_for(&e))?;
    if metadata.is_dir() {
        // Removing a directory tree over HTTP is not something this server
        // offers, whatever the route allows.
        return Err(403);
    }
    fs::remove_file(path).map_err(|e| status_for(&e))?;
    Ok(Reply::immediate(Response::new(204)))
}

/// Store an uploaded body under `dir`.
///
/// Handles both shapes the audit uses: `multipart/form-data` (a browser form or
/// `curl -F`) and a raw body (`curl --data-binary`).
pub fn upload(dir: &Path, request: &Request, url_path: &str) -> Result<Reply, u16> {
    if request.body.is_empty() {
        return Err(400);
    }

    let content_type = request.headers.get("content-type").unwrap_or("");
    let stored = match multipart::boundary_of(content_type) {
        Some(boundary) => store_multipart(dir, &request.body, &boundary)?,
        None => vec![store_raw(dir, request, url_path)?],
    };

    let first = match stored.first() {
        Some(name) => name,
        // A multipart body with no file parts at all.
        None => return Err(400),
    };

    // The URL may already name the file (`POST /uploads/note.txt`) or only the
    // directory (`POST /uploads/`). Appending unconditionally would produce
    // `/uploads/note.txt/note.txt`.
    let base = if url_path.ends_with('/') {
        url_path.trim_end_matches('/').to_string()
    } else {
        match url_path.rfind('/') {
            Some(slash) => url_path[..slash].to_string(),
            None => String::new(),
        }
    };
    let location = format!("{base}/{first}");
    let names = stored.join("\n");
    Ok(Reply::immediate(
        Response::with_body(
            201,
            "text/plain; charset=utf-8",
            format!("stored:\n{names}\n").into_bytes(),
        )
        // 201 must say where the thing it created lives.
        .header("Location", &location),
    ))
}

fn store_multipart(dir: &Path, body: &[u8], boundary: &str) -> Result<Vec<String>, u16> {
    let parts = multipart::parse(body, boundary)?;
    let mut stored = Vec::new();
    for part in parts {
        // Parts without a filename are ordinary form fields, not uploads.
        let name = match part.safe_filename() {
            Some(name) => name,
            None if part.filename.is_some() => {
                // A filename was sent but nothing safe survived sanitising
                // (`..`, a dotfile, control characters). Refuse rather than
                // invent a name.
                return Err(400);
            }
            None => continue,
        };
        write_file(&dir.join(&name), &part.data)?;
        stored.push(name);
    }
    Ok(stored)
}

fn store_raw(dir: &Path, request: &Request, url_path: &str) -> Result<String, u16> {
    // A raw POST to `/uploads/report.txt` names the file; a POST to the
    // directory itself does not, so one is generated.
    let from_url = url_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .and_then(multipart::sanitize_filename);

    let name = match from_url {
        Some(name) => name,
        None => generated_name(request),
    };
    write_file(&dir.join(&name), &request.body)?;
    Ok(name)
}

/// A name for an upload that did not supply one: seconds since the epoch plus
/// an extension guessed from the content type.
fn generated_name(request: &Request) -> String {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let extension = match request.headers.get("content-type").unwrap_or("") {
        t if t.starts_with("text/plain") => "txt",
        t if t.starts_with("text/html") => "html",
        t if t.starts_with("application/json") => "json",
        t if t.starts_with("image/png") => "png",
        t if t.starts_with("image/jpeg") => "jpg",
        _ => "bin",
    };
    format!("upload-{stamp}.{extension}")
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), u16> {
    fs::write(path, data).map_err(|e| status_for(&e))
}

/// Generate an HTML listing for a directory.
pub fn listing(dir: &Path, url_path: &str, head_only: bool) -> Result<Reply, u16> {
    let entries = fs::read_dir(dir).map_err(|e| status_for(&e))?;

    let mut rows: Vec<(bool, String, u64, Option<SystemTime>)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Hidden files are not advertised. Anyone who knows the name can still
        // request it; this is tidiness, not a security boundary.
        if name.starts_with('.') {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            // A file removed between read_dir and stat: skip it rather than
            // fail the whole listing.
            Err(_) => continue,
        };
        rows.push((metadata.is_dir(), name, metadata.len(), metadata.modified().ok()));
    }

    // Directories first, then case-insensitive by name — the ordering every
    // file manager uses, and stable across runs unlike readdir order.
    rows.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    let mut html = String::with_capacity(1024 + rows.len() * 128);
    html.push_str(&format!(
        "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Index of {}</title>\n\
         <style>\
         body{{margin:0;padding:2rem 1.5rem;background:#10151c;color:#e6edf3;\
         font:15px/1.6 system-ui,-apple-system,\"Segoe UI\",sans-serif}}\
         main{{max-width:52rem;margin:0 auto}}\
         h1{{font-size:1.2rem;margin:0 0 1rem;font-family:ui-monospace,Menlo,monospace}}\
         table{{width:100%;border-collapse:collapse}}\
         th,td{{text-align:left;padding:.4rem .6rem;border-bottom:1px solid #2a3441}}\
         th{{color:#8b98a5;font-weight:600;font-size:.8rem;text-transform:uppercase;letter-spacing:.04em}}\
         td.size,td.date{{color:#8b98a5;white-space:nowrap}}\
         td.size{{text-align:right}}\
         a{{color:#58a6ff;text-decoration:none}}a:hover{{text-decoration:underline}}\
         footer{{margin-top:1.5rem;color:#8b98a5;font-size:.8rem}}\
         </style></head>\n<body><main>\n<h1>Index of {}</h1>\n\
         <table><thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead><tbody>\n",
        escape_html(url_path),
        escape_html(url_path)
    ));

    // A parent link, except at the route's own root where it would leave it.
    if url_path != "/" {
        html.push_str("<tr><td><a href=\"../\">../</a></td><td class=\"size\">—</td><td class=\"date\">—</td></tr>\n");
    }

    for (is_dir, name, size, modified) in &rows {
        // The href is percent-encoded (a name may contain spaces or `#`); the
        // text is HTML-escaped (it may contain `<`). Two different encodings
        // for two different contexts — using one for both is the classic bug.
        let href = format!(
            "{}{}",
            encode_segment(name),
            if *is_dir { "/" } else { "" }
        );
        let shown = format!("{}{}", escape_html(name), if *is_dir { "/" } else { "" });
        let size_cell = if *is_dir { "—".to_string() } else { human_size(*size) };
        let date_cell = modified.map(http_date).unwrap_or_else(|| "—".to_string());
        html.push_str(&format!(
            "<tr><td><a href=\"{href}\">{shown}</a></td>\
             <td class=\"size\">{size_cell}</td><td class=\"date\">{date_cell}</td></tr>\n"
        ));
    }

    html.push_str(&format!(
        "</tbody></table>\n<footer>{} item(s) — LocalServer</footer>\n</main></body></html>\n",
        rows.len()
    ));

    Ok(Reply::immediate(
        Response::with_body(200, "text/html; charset=utf-8", html.into_bytes())
            .head_only(head_only),
    ))
}

/// Escape text for HTML content and attribute values.
fn escape_html(input: &str) -> String {
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

/// Percent-encode one path segment for use in an href.
fn encode_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Human-readable size, so a listing is readable at a glance.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Directory used by [`upload`] must exist and be writable; checked at startup,
/// re-checked here because it may have changed since.
pub fn upload_dir_usable(dir: &PathBuf) -> bool {
    fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::http::request::{Progress, RequestParser};
    use std::io::Read;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir()
                .join(format!("localserver-h-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn request(raw: &[u8]) -> Request {
        let mut parser = RequestParser::new(1024 * 1024);
        assert_eq!(parser.feed(raw), Progress::Complete);
        parser.take().unwrap()
    }

    /// Collect a reply's body, from wherever it lives: small files travel
    /// inline with the headers, large ones are streamed from disk.
    fn collect(reply: &mut Reply) -> Vec<u8> {
        match reply.body.as_mut() {
            Some(body) => {
                let mut out = Vec::new();
                body.file.read_to_end(&mut out).unwrap();
                out
            }
            None => reply.response.body.clone(),
        }
    }

    // --- GET ([A-17]) --------------------------------------------------------

    #[test]
    fn serves_a_file_with_its_length_and_type() {
        let dir = TempDir::new("get");
        fs::write(dir.path.join("page.html"), b"<h1>hi</h1>").unwrap();
        let mut reply = file(&dir.path.join("page.html"), false).unwrap();
        assert_eq!(reply.response.status, 200);
        assert_eq!(reply.response.get_header("Content-Length"), Some("11"));
        assert_eq!(
            reply.response.get_header("Content-Type"),
            Some("text/html; charset=utf-8")
        );
        assert!(reply.response.get_header("Last-Modified").is_some());
        assert_eq!(collect(&mut reply), b"<h1>hi</h1>");
    }

    /// A file small enough to fit one chunk travels inline, saving two
    /// syscalls and two epoll re-arms on the traffic a static site is mostly
    /// made of.
    #[test]
    fn a_small_file_is_sent_inline_with_its_headers() {
        let dir = TempDir::new("inline");
        fs::write(dir.path.join("small.txt"), b"tiny").unwrap();
        let reply = file(&dir.path.join("small.txt"), false).unwrap();
        assert!(reply.body.is_none(), "no streaming needed for a small file");
        assert_eq!(reply.response.body, b"tiny");
        assert_eq!(reply.response.get_header("Content-Length"), Some("4"));
    }

    /// Above one chunk the body stays on disk until the socket is ready for it;
    /// that is what keeps memory flat under load.
    #[test]
    fn a_large_file_body_is_not_read_into_the_response() {
        let dir = TempDir::new("stream");
        // Deliberately larger than one chunk, so the streaming path is taken.
        let size = STREAM_CHUNK * 3;
        fs::write(dir.path.join("big.bin"), vec![b'x'; size]).unwrap();
        let reply = file(&dir.path.join("big.bin"), false).unwrap();
        assert!(reply.response.body.is_empty(), "the body must not be buffered");
        assert_eq!(reply.response.get_header("Content-Length"), Some(size.to_string().as_str()));
        assert_eq!(reply.body.as_ref().unwrap().remaining, size as u64);
    }

    #[test]
    fn head_reports_the_length_without_a_body() {
        let dir = TempDir::new("head");
        fs::write(dir.path.join("a.txt"), b"0123456789").unwrap();
        let reply = file(&dir.path.join("a.txt"), true).unwrap();
        assert_eq!(reply.response.get_header("Content-Length"), Some("10"));
        assert!(reply.body.is_none(), "HEAD must not stream a body");
        let text = String::from_utf8_lossy(&reply.response.to_bytes(true)).to_string();
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_missing_file_is_404_and_a_directory_is_403() {
        let dir = TempDir::new("missing");
        assert_eq!(file(&dir.path.join("nope.txt"), false).err(), Some(404));
        assert_eq!(file(&dir.path, false).err(), Some(403));
    }

    // --- DELETE ([A-19]) -----------------------------------------------------

    #[test]
    fn deletes_a_file_and_answers_204() {
        let dir = TempDir::new("delete");
        let target = dir.path.join("gone.txt");
        fs::write(&target, b"bye").unwrap();
        let reply = delete(&target).unwrap();
        assert_eq!(reply.response.status, 204);
        assert!(!target.exists(), "the file should be gone");
        // 204 carries no body and announces no length.
        let text = String::from_utf8_lossy(&reply.response.to_bytes(true)).to_string();
        assert!(!text.contains("Content-Length"));
    }

    #[test]
    fn deleting_a_missing_file_is_404_and_a_directory_is_403() {
        let dir = TempDir::new("delete2");
        assert_eq!(delete(&dir.path.join("absent")).err(), Some(404));
        assert_eq!(delete(&dir.path).err(), Some(403));
    }

    // --- POST ([A-18], [A-21]) ----------------------------------------------

    #[test]
    fn stores_a_raw_body_under_the_name_from_the_url() {
        let dir = TempDir::new("raw");
        let request = request(b"POST /uploads/note.txt HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello");
        let reply = upload(&dir.path, &request, "/uploads/note.txt").unwrap();
        assert_eq!(reply.response.status, 201);
        assert_eq!(reply.response.get_header("Location"), Some("/uploads/note.txt"));
        assert_eq!(fs::read(dir.path.join("note.txt")).unwrap(), b"hello");
    }

    #[test]
    fn generates_a_name_when_the_url_is_a_directory() {
        let dir = TempDir::new("raw2");
        let request = request(b"POST /uploads/ HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nhi");
        let reply = upload(&dir.path, &request, "/uploads/").unwrap();
        assert_eq!(reply.response.status, 201);
        let entries: Vec<_> = fs::read_dir(&dir.path).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let name = entries[0].file_name().to_string_lossy().to_string();
        assert!(name.starts_with("upload-") && name.ends_with(".txt"), "got {name}");
    }

    /// [A-21] in miniature: bytes in must equal bytes out, including the ones
    /// that break a text-oriented parser.
    #[test]
    fn a_multipart_upload_survives_byte_for_byte() {
        let dir = TempDir::new("multipart");
        let payload: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x00, 0xFF, 0x0D, 0x0A];
        let mut body = Vec::new();
        body.extend_from_slice(b"--BND\r\n");
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"f\"; filename=\"logo.png\"\r\n\r\n");
        body.extend_from_slice(&payload);
        body.extend_from_slice(b"\r\n--BND--\r\n");

        let mut raw = format!(
            "POST /uploads/ HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary=BND\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(&body);

        let reply = upload(&dir.path, &request(&raw), "/uploads/").unwrap();
        assert_eq!(reply.response.status, 201);
        assert_eq!(reply.response.get_header("Location"), Some("/uploads/logo.png"));
        assert_eq!(fs::read(dir.path.join("logo.png")).unwrap(), payload);
    }

    /// A filename that sanitises to nothing is refused rather than silently
    /// renamed, so a client never believes it stored something it did not.
    #[test]
    fn an_unsafe_upload_filename_is_rejected() {
        let dir = TempDir::new("unsafe");
        let mut body = Vec::new();
        body.extend_from_slice(b"--B\r\n");
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"f\"; filename=\"..\"\r\n\r\n");
        body.extend_from_slice(b"payload");
        body.extend_from_slice(b"\r\n--B--\r\n");
        let mut raw = format!(
            "POST /uploads/ HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary=B\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(&body);
        assert_eq!(upload(&dir.path, &request(&raw), "/uploads/").err(), Some(400));
    }

    /// A traversing filename keeps only its final component and lands inside
    /// the upload directory.
    #[test]
    fn a_traversing_upload_filename_stays_in_the_directory() {
        let dir = TempDir::new("traverse");
        let mut body = Vec::new();
        body.extend_from_slice(b"--B\r\n");
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"f\"; filename=\"../../evil.txt\"\r\n\r\n",
        );
        body.extend_from_slice(b"payload");
        body.extend_from_slice(b"\r\n--B--\r\n");
        let mut raw = format!(
            "POST /uploads/ HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary=B\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(&body);

        upload(&dir.path, &request(&raw), "/uploads/").unwrap();
        assert!(dir.path.join("evil.txt").is_file());
        assert!(!dir.path.parent().unwrap().join("evil.txt").exists());
    }

    #[test]
    fn an_empty_upload_is_rejected() {
        let dir = TempDir::new("empty");
        let request = request(b"POST /uploads/ HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(upload(&dir.path, &request, "/uploads/").err(), Some(400));
    }

    // --- directory listing ([A-26]) -----------------------------------------

    #[test]
    fn lists_a_directory_with_sizes_and_a_parent_link() {
        let dir = TempDir::new("listing");
        fs::write(dir.path.join("b.txt"), vec![b'x'; 2048]).unwrap();
        fs::create_dir(dir.path.join("adir")).unwrap();

        let reply = listing(&dir.path, "/files/", false).unwrap();
        let html = String::from_utf8_lossy(&reply.response.body).to_string();
        assert_eq!(reply.response.status, 200);
        assert!(html.contains("Index of /files/"));
        assert!(html.contains("href=\"../\""), "needs a parent link");
        // Directories sort first and are marked with a trailing slash.
        let dir_at = html.find("adir/").unwrap();
        let file_at = html.find("b.txt").unwrap();
        assert!(dir_at < file_at, "directories should sort first");
        assert!(html.contains("2.0 KB"));
    }

    /// At the route's own root a `..` link would leave the served tree.
    #[test]
    fn the_route_root_has_no_parent_link() {
        let dir = TempDir::new("listing-root");
        let reply = listing(&dir.path, "/", false).unwrap();
        let html = String::from_utf8_lossy(&reply.response.body).to_string();
        assert!(!html.contains("href=\"../\""));
    }

    /// Two encodings for two contexts: the href is percent-encoded, the text is
    /// HTML-escaped. Using one for both is the classic bug, and lets a crafted
    /// file name inject markup.
    #[test]
    fn file_names_are_escaped_for_html_and_encoded_for_urls() {
        let dir = TempDir::new("escape");
        fs::write(dir.path.join("a b&c.txt"), b"x").unwrap();
        fs::write(dir.path.join("<script>.txt"), b"x").unwrap();

        let reply = listing(&dir.path, "/", false).unwrap();
        let html = String::from_utf8_lossy(&reply.response.body).to_string();

        assert!(html.contains("a%20b%26c.txt"), "href must be percent-encoded");
        assert!(html.contains("a b&amp;c.txt"), "text must be HTML-escaped");
        assert!(!html.contains("<script>.txt"), "markup must not survive into the page");
        assert!(html.contains("&lt;script&gt;.txt"));
    }

    #[test]
    fn hidden_files_are_not_listed() {
        let dir = TempDir::new("hidden");
        fs::write(dir.path.join(".secret"), b"x").unwrap();
        fs::write(dir.path.join("visible.txt"), b"x").unwrap();
        let reply = listing(&dir.path, "/", false).unwrap();
        let html = String::from_utf8_lossy(&reply.response.body).to_string();
        assert!(!html.contains(".secret"));
        assert!(html.contains("visible.txt"));
    }

    #[test]
    fn formats_sizes_readably() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn escapes_and_encodes_correctly() {
        assert_eq!(escape_html("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
        assert_eq!(encode_segment("a b.txt"), "a%20b.txt");
        assert_eq!(encode_segment("plain-name_1.txt"), "plain-name_1.txt");
        assert_eq!(encode_segment("#frag?q"), "%23frag%3Fq");
    }
}
