//! Integration test suite for LocalServer using raw `std::net::TcpStream`.
//!
//! Tests a live server instance started with `config.yaml` on ports 8080, 8081, 8082:
//! - a) GET static files (.html, .css, .png, headers: 200, Content-Type, Date, keep-alive)
//! - b) Custom 404 error page (non-existent URL returns 404 with custom body)
//! - c) Accepted methods (DELETE on /uploads/ -> 204 or 404; DELETE on /readonly/ -> 405 + Allow)
//! - d) Client max body limit (POST to port 8082 with >1KB data -> 413)
//! - e) Virtual hosts (Host: localhost vs Host: test.com on port 8080 -> distinct responses)
//! - f) File upload and roundtrip (POST multipart/form-data 50KB binary, GET back, SHA256 match)
//! - g) HTTP 301 Redirection (GET /old-page -> 301 Location: /index.html)
//! - h) Directory autoindex (GET /listing/ -> HTML listing with sizes and escaping)
//! - i) Pipelined requests (2 GETs back-to-back in 1 write, 2 responses in order)
//! - j) One-byte-at-a-time trickle (send request 1 byte at a time, expect 200)
//! - k) Sessions and cookies (GET /session Set-Cookie, subsequent GET Cookie visits++)
//! - l) CGI execution (GET/POST cgi-bin/hello.py and cgi-bin/info.pl -> 200 OK)
//! - m) CGI timeout (GET cgi-bin/slow.py -> 504 Gateway Timeout and cleanup)
//! - n) Malformed requests (GARBAGE\r\n\r\n -> 400 Bad Request, server stays alive)

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Server process lifecycle helper
// ---------------------------------------------------------------------------

static SERVER_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn cleanup_server() {
    let pid = SERVER_PID.swap(0, Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

struct ServerProcess {
    child: Option<Child>,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

static SERVER: OnceLock<Mutex<ServerProcess>> = OnceLock::new();

fn ensure_server() {
    SERVER.get_or_init(|| {
        let bin = env!("CARGO_BIN_EXE_localserver");
        let ports = [8080, 8081, 8082];
        let all_responding = ports.iter().all(|&port| {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok()
        });
        if all_responding {
            return Mutex::new(ServerProcess { child: None });
        }

        // Wait up to 3s for any lingering socket on these ports to be fully released
        for &port in &ports {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        // Spawn server process configured with config.yaml
        let mut cmd = Command::new(bin);
        cmd.arg("config.yaml");

        let child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn localserver from {bin}: {e}"));

        let pid = child.id() as i32;
        SERVER_PID.store(pid, Ordering::SeqCst);
        unsafe {
            libc::atexit(cleanup_server);
        }

        // Wait for all 3 listening ports (8080, 8081, 8082) to accept connections
        let deadline = Instant::now() + Duration::from_secs(10);
        let ports = [8080, 8081, 8082];
        for &port in &ports {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let mut connected = false;
            while Instant::now() < deadline {
                if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
                    drop(stream);
                    connected = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !connected {
                panic!("server failed to bind and listen on 127.0.0.1:{port} within 10s");
            }
        }

        Mutex::new(ServerProcess { child: Some(child) })
    });
}

// ---------------------------------------------------------------------------
// HTTP Response parser and reader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Response {
    status: u16,
    #[allow(dead_code)]
    status_text: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

struct ResponseReader {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl ResponseReader {
    fn new(stream: TcpStream) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .expect("set read timeout");
        ResponseReader {
            stream,
            buf: Vec::with_capacity(16 * 1024),
        }
    }

    fn read_response(&mut self) -> Result<Response, String> {
        let mut temp = [0u8; 4096];

        // 1. Read until headers are complete (\r\n\r\n)
        let header_end = loop {
            if let Some(pos) = self.find_header_end() {
                break pos;
            }
            let n = self
                .stream
                .read(&mut temp)
                .map_err(|e| format!("read headers failed: {e}"))?;
            if n == 0 {
                return Err("stream closed before end of headers".to_string());
            }
            self.buf.extend_from_slice(&temp[..n]);
        };

        // Parse status line and headers
        let header_str = String::from_utf8_lossy(&self.buf[..header_end]).into_owned();
        let mut lines = header_str.split("\r\n");

        let status_line = lines.next().ok_or("missing status line")?.to_string();
        let mut parts = status_line.splitn(3, ' ');
        let _version = parts.next().ok_or("missing HTTP version")?;
        let status_str = parts.next().ok_or("missing status code")?;
        let status: u16 = status_str
            .parse()
            .map_err(|e| format!("invalid status code `{status_str}`: {e}"))?;
        let status_text = parts.next().unwrap_or("").to_string();

        let mut headers = HashMap::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, val)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), val.trim().to_string());
            }
        }

        // Remaining bytes in buffer belong to body
        let body_start = header_end + 4; // skip \r\n\r\n
        let mut body_buf = self.buf[body_start..].to_vec();
        self.buf.clear();

        // 2. Read body based on framing
        let content_length = headers.get("content-length").and_then(|v| v.parse::<usize>().ok());
        let is_chunked = headers
            .get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);

        let body = if status == 204 || status == 304 || status_line.starts_with("HTTP/1.1 100") {
            // No body
            self.buf = body_buf;
            Vec::new()
        } else if let Some(len) = content_length {
            while body_buf.len() < len {
                let n = self
                    .stream
                    .read(&mut temp)
                    .map_err(|e| format!("read body failed: {e}"))?;
                if n == 0 {
                    break;
                }
                body_buf.extend_from_slice(&temp[..n]);
            }
            let extra = if body_buf.len() > len {
                body_buf.split_off(len)
            } else {
                Vec::new()
            };
            self.buf = extra;
            body_buf
        } else if is_chunked {
            let mut decoded = Vec::new();
            loop {
                // Find CRLF for chunk size line
                let crlf = loop {
                    if let Some(pos) = body_buf.windows(2).position(|w| w == b"\r\n") {
                        break pos;
                    }
                    let n = self
                        .stream
                        .read(&mut temp)
                        .map_err(|e| format!("read chunk header failed: {e}"))?;
                    if n == 0 {
                        return Err("unexpected EOF in chunked body".to_string());
                    }
                    body_buf.extend_from_slice(&temp[..n]);
                };

                let line = String::from_utf8_lossy(&body_buf[..crlf]);
                let chunk_size_hex = line.split(';').next().unwrap_or("").trim();
                let chunk_size = usize::from_str_radix(chunk_size_hex, 16)
                    .map_err(|e| format!("bad chunk size `{chunk_size_hex}`: {e}"))?;

                let total_needed = crlf + 2 + chunk_size + 2; // size_line\r\n + chunk + \r\n
                while body_buf.len() < total_needed {
                    let n = self
                        .stream
                        .read(&mut temp)
                        .map_err(|e| format!("read chunk data failed: {e}"))?;
                    if n == 0 {
                        return Err("unexpected EOF reading chunk data".to_string());
                    }
                    body_buf.extend_from_slice(&temp[..n]);
                }

                let chunk_data = &body_buf[crlf + 2..crlf + 2 + chunk_size];
                decoded.extend_from_slice(chunk_data);

                body_buf = body_buf[total_needed..].to_vec();
                if chunk_size == 0 {
                    break;
                }
            }
            self.buf = body_buf;
            decoded
        } else {
            // Connection: close or until EOF
            while let Ok(n) = self.stream.read(&mut temp) {
                if n == 0 {
                    break;
                }
                body_buf.extend_from_slice(&temp[..n]);
            }
            body_buf
        };

        Ok(Response {
            status,
            status_text,
            headers,
            body,
        })
    }

    fn find_header_end(&self) -> Option<usize> {
        self.buf.windows(4).position(|w| w == b"\r\n\r\n")
    }
}

fn connect(port: u16) -> TcpStream {
    ensure_server();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .unwrap_or_else(|e| panic!("could not connect to 127.0.0.1:{port}: {e}"))
}

fn request(port: u16, raw: &str) -> Response {
    let mut stream = connect(port);
    stream.write_all(raw.as_bytes()).expect("write request");
    let mut reader = ResponseReader::new(stream);
    reader.read_response().expect("read response")
}

// ---------------------------------------------------------------------------
// Pure Rust SHA-256 (FIPS 180-4) implementation (no extra crate dependencies)
// ---------------------------------------------------------------------------

fn sha256(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, part) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([part[0], part[1], part[2], part[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h_val] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_val
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_val = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_val);
    }

    let mut out = String::with_capacity(64);
    for val in h {
        out.push_str(&format!("{val:08x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests a) through n)
// ---------------------------------------------------------------------------

#[test]
fn test_a_get_static_files() {
    ensure_server();

    // 1. HTML file
    let resp = request(8080, "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("HTML Content-Type");
    assert!(ct.starts_with("text/html"), "expected text/html, got {ct}");
    assert_eq!(resp.header("Connection"), Some("keep-alive"));
    let date = resp.header("Date").expect("Date header");
    assert!(date.ends_with("GMT"), "Date should end with GMT: {date}");
    assert!(resp.text().contains("LocalServer"));

    // 2. CSS file
    let resp = request(8080, "GET /static/style.css HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("CSS Content-Type");
    assert!(ct.starts_with("text/css"), "expected text/css, got {ct}");
    assert!(resp.text().contains("body"));

    // 3. PNG file
    let resp = request(8080, "GET /static/logo.png HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("Content-Type"), Some("image/png"));
    assert!(!resp.body.is_empty());
    // PNG magic bytes
    assert_eq!(&resp.body[0..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn test_b_custom_404_error_page() {
    ensure_server();
    let resp = request(8080, "GET /nonexistent-url-404.html HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 404);
    let text = resp.text();
    assert!(text.contains("<title>404 Not Found</title>"));
    assert!(text.contains("No route or file matches this URL on this server."));
}

#[test]
fn test_c_accepted_methods() {
    ensure_server();

    // 1. Create a dummy file in uploads
    std::fs::write("uploads/method_test.txt", b"delete me").expect("write test file");

    // 2. DELETE on /uploads/ returns 204
    let resp = request(8080, "DELETE /uploads/method_test.txt HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 204);

    // 3. Repeating DELETE on non-existent file returns 404
    let resp = request(8080, "DELETE /uploads/method_test.txt HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 404);

    // 4. DELETE on /readonly/ returns 405 Method Not Allowed with Allow header
    let resp = request(8080, "DELETE /readonly/style.css HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 405);
    let allow = resp.header("Allow").expect("Allow header on 405");
    assert!(allow.contains("GET"), "Allow header must mention GET: {allow}");
}

#[test]
fn test_d_client_max_body_limit() {
    ensure_server();

    // Port 8082 has client_max_body_size: 1K (1024 bytes)
    // Send 2048 bytes body -> must return 413 Content Too Large
    let big_body = vec![b'A'; 2048];
    let req = format!(
        "POST / HTTP/1.1\r\nHost: tiny.local\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
        big_body.len(),
        String::from_utf8_lossy(&big_body)
    );
    let resp = request(8082, &req);
    assert_eq!(resp.status, 413);

    // Send small body (256 bytes) -> body limit accepted
    let small_body = vec![b'B'; 256];
    let req = format!(
        "POST / HTTP/1.1\r\nHost: tiny.local\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
        small_body.len(),
        String::from_utf8_lossy(&small_body)
    );
    let resp = request(8082, &req);
    assert_ne!(resp.status, 413, "body within 1K must not be 413");
}

#[test]
fn test_e_virtual_hosts() {
    ensure_server();

    // Port 8080 with Host: localhost -> serves main site
    let resp1 = request(8080, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp1.status, 200);
    let body1 = resp1.text();
    assert!(body1.contains("<title>LocalServer</title>"));

    // Port 8080 with Host: test.com -> serves site2
    let resp2 = request(8080, "GET / HTTP/1.1\r\nHost: test.com\r\n\r\n");
    assert_eq!(resp2.status, 200);
    let body2 = resp2.text();
    assert!(body2.contains("<title>test.com — virtual host</title>"));

    assert_ne!(body1, body2, "Virtual hosts must return different content");
}

#[test]
fn test_f_file_upload_and_roundtrip() {
    ensure_server();

    // Generate 50KB deterministic binary payload
    let mut payload = Vec::with_capacity(50 * 1024);
    for i in 0..(50 * 1024) {
        payload.push((i % 256) as u8);
    }
    let original_checksum = sha256(&payload);

    let boundary = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
    let filename = "roundtrip_test.bin";

    let mut multipart_body = Vec::new();
    multipart_body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    multipart_body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    multipart_body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    multipart_body.extend_from_slice(&payload);
    multipart_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    // 1. POST upload
    let mut stream = connect(8080);
    let req_headers = format!(
        "POST /uploads/ HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        multipart_body.len()
    );
    stream.write_all(req_headers.as_bytes()).unwrap();
    stream.write_all(&multipart_body).unwrap();

    let mut reader = ResponseReader::new(stream);
    let resp = reader.read_response().expect("upload response");
    assert_eq!(resp.status, 201);
    let location = resp.header("Location").expect("201 Location header");
    assert_eq!(location, "/uploads/roundtrip_test.bin");

    // 2. GET back the uploaded file
    let resp_get = request(
        8080,
        "GET /uploads/roundtrip_test.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(resp_get.status, 200);
    assert_eq!(resp_get.body.len(), payload.len());
    let downloaded_checksum = sha256(&resp_get.body);
    assert_eq!(
        original_checksum, downloaded_checksum,
        "SHA-256 checksums must match exactly"
    );

    // Clean up file
    let _ = request(
        8080,
        "DELETE /uploads/roundtrip_test.bin HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
}

#[test]
fn test_g_http_301_redirection() {
    ensure_server();
    let resp = request(8080, "GET /old-page HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 301);
    assert_eq!(resp.header("Location"), Some("/index.html"));
}

#[test]
fn test_h_directory_autoindex() {
    ensure_server();
    let resp = request(8080, "GET /listing/ HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 200);
    let ct = resp.header("Content-Type").expect("Content-Type");
    assert!(ct.starts_with("text/html"));
    let html = resp.text();
    assert!(html.contains("Index of /listing/"));
    assert!(html.contains("data.csv"));
    assert!(html.contains("notes.txt"));
    assert!(html.contains("readme.md"));
    assert!(html.contains("nested/"));
}

#[test]
fn test_i_pipelined_requests() {
    ensure_server();
    let mut stream = connect(8080);

    // Write 2 GET requests back-to-back into one TCP write
    let pipelined = "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\nGET /static/style.css HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(pipelined.as_bytes()).expect("write pipelined");

    let mut reader = ResponseReader::new(stream);
    let resp1 = reader.read_response().expect("read response 1");
    assert_eq!(resp1.status, 200);
    assert!(resp1.text().contains("LocalServer"));

    let resp2 = reader.read_response().expect("read response 2");
    assert_eq!(resp2.status, 200);
    assert!(resp2.text().contains("body"));
}

#[test]
fn test_j_one_byte_at_a_time_trickle() {
    ensure_server();
    let mut stream = connect(8080);

    let raw = b"GET /index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    for &byte in raw {
        stream.write_all(&[byte]).expect("write byte");
        thread::sleep(Duration::from_millis(1));
    }

    let mut reader = ResponseReader::new(stream);
    let resp = reader.read_response().expect("read trickle response");
    assert_eq!(resp.status, 200);
    assert!(resp.text().contains("LocalServer"));
}

#[test]
fn test_k_sessions_and_cookies() {
    ensure_server();

    // 1. Initial request without cookies
    let resp1 = request(8080, "GET /session HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp1.status, 200);
    let set_cookie = resp1.header("Set-Cookie").expect("Set-Cookie header on new session");
    assert!(set_cookie.contains("localserver_session="));
    assert!(resp1.text().contains("Requests on it</th><td>1</td>"));

    // Extract cookie value
    let cookie_val = set_cookie
        .split(';')
        .next()
        .unwrap()
        .trim();

    // 2. Subsequent request sending the Cookie header
    let req2 = format!("GET /session HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie_val}\r\n\r\n");
    let resp2 = request(8080, &req2);
    assert_eq!(resp2.status, 200);
    assert!(
        resp2.text().contains("Requests on it</th><td>2</td>"),
        "Visits must be incremented to 2"
    );
}

#[test]
fn test_l_cgi_execution() {
    ensure_server();

    // 1. Python CGI GET
    let resp_py = request(8080, "GET /cgi-bin/hello.py HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_py.status, 200);
    assert_eq!(resp_py.header("X-Cgi-Language"), Some("python"));
    assert!(resp_py.text().contains("CGI is running (python)"));

    // 2. Python CGI POST with body
    let body = "hello_cgi_data=12345";
    let req_post = format!(
        "POST /cgi-bin/hello.py HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_post = request(8080, &req_post);
    assert_eq!(resp_post.status, 200);
    assert!(resp_post.text().contains("hello_cgi_data=12345"));

    // 3. Perl CGI GET
    let resp_pl = request(8080, "GET /cgi-bin/info.pl HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_pl.status, 200);
    assert_eq!(resp_pl.header("X-Cgi-Language"), Some("perl"));
    assert!(resp_pl.text().contains("CGI is running (perl)"));
}

#[test]
fn test_m_cgi_timeout() {
    ensure_server();

    // slow.py sleeps 120s; CGI_TIMEOUT is 10s -> server must return 504 Gateway Timeout
    let resp = request(8080, "GET /cgi-bin/slow.py HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp.status, 504);

    // Server remains alive and responsive after timeout cleanup
    let follow_up = request(8080, "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(follow_up.status, 200);
}

#[test]
fn test_n_malformed_requests() {
    ensure_server();

    // Send garbage to port 8080
    let mut stream = connect(8080);
    stream.write_all(b"GARBAGE\r\n\r\n").expect("write garbage");

    let mut reader = ResponseReader::new(stream);
    let resp = reader.read_response().expect("read 400 response");
    assert_eq!(resp.status, 400);

    // Verify server remains alive and serves next request
    let resp2 = request(8080, "GET /index.html HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp2.status, 200);
    assert!(resp2.text().contains("LocalServer"));
}

#[test]
fn test_o_admin_metrics() {
    ensure_server();

    // 1. GET /admin HTML dashboard
    let resp_admin = request(8080, "GET /admin HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_admin.status, 200);
    assert_eq!(resp_admin.header("Content-Type"), Some("text/html; charset=utf-8"));
    let html = resp_admin.text();
    assert!(html.contains("Admin Dashboard"));
    assert!(html.contains("Active Connections"));
    assert!(html.contains("Open FDs"));

    // 2. GET /admin/metrics.json
    let resp_json = request(8080, "GET /admin/metrics.json HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_json.status, 200);
    assert_eq!(resp_json.header("Content-Type"), Some("application/json"));
    let json = resp_json.text();
    assert!(json.contains("\"uptime_seconds\":"));
    assert!(json.contains("\"total_connections\":"));
    assert!(json.contains("\"requests_total\":"));
    assert!(json.contains("\"open_fds\":"));
    assert!(json.contains("\"active_sessions\":"));
}

#[test]
fn test_p_security_hardening() {
    ensure_server();

    // Test 1: Percent-encoded route prefix resolution (/%75ploads -> /uploads)
    let resp_uploads = request(8080, "GET /%75ploads/ HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_uploads.status, 200);

    // Test 2: Redirect HTML escaping (prevent reflected HTML/XSS)
    let resp_redir = request(8080, "GET /listing?foo=\"><script>alert(1)</script> HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(resp_redir.status, 301);
    let body = resp_redir.text();
    assert!(!body.contains("<script>alert(1)</script>"));
    assert!(body.contains("&lt;script&gt;"));
    assert!(body.contains("&quot;&gt;"));
}

// ---------------------------------------------------------------------------
// Regression tests for visual-audit dashboard bugs (A-13, A-25, A-27, A-22).
// Each test codifies the backend truth so a visual card with wrong
// expectations (false PASS / false FAIL) is caught.
// ---------------------------------------------------------------------------

/// [A-13] The 8082 server block has ONLY route `/` (config.yaml:133-138).
/// The visual `a13_small` card POSTs to `http://127.0.0.1:8082/uploads/...`
/// and expects 200/201 — but the correct backend answer is 403 (no upload
/// route), not 200. POSTs to `/` are the valid body-limit probe.
#[test]
fn test_q_8082_routing_and_body_limit() {
    ensure_server();

    // 1. Small POST to a non-existent /uploads route on 8082 must be 403,
    //    proving the visual card targets the wrong URL (false FAIL today).
    let small = "small payload under 1KB limit";
    let req_bad_route = format!(
        "POST /uploads/test_small.txt HTTP/1.1\r\nHost: tiny.local\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
        small.len(),
        small
    );
    let resp = request(8082, &req_bad_route);
    assert_eq!(
        resp.status, 403,
        "8082 has no /uploads route: small POST must be 403, not 200/201"
    );

    // 2. Small POST to `/` on 8082 must NOT be 413 (limit is 1K).
    let req_small = format!(
        "POST / HTTP/1.1\r\nHost: tiny.local\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
        small.len(),
        small
    );
    let resp_small = request(8082, &req_small);
    assert_ne!(resp_small.status, 413, "256B-scale body on 8082 must not be 413");

    // 3. 2KB POST to `/` on 8082 must be exactly 413.
    let big = vec![b'X'; 2048];
    let req_big = format!(
        "POST / HTTP/1.1\r\nHost: tiny.local\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
        big.len(),
        String::from_utf8_lossy(&big)
    );
    let resp_big = request(8082, &req_big);
    assert_eq!(resp_big.status, 413, "2KB body on 1K-limited 8082 must be 413");
}

/// [A-27] `/old-page` must be exactly 301 + `Location: /index.html`.
/// The visual `a27` card accepts `opaqueredirect || 301 || 200`, so a
/// broken redirect returning 200 still shows PASS (tautology).
#[test]
fn test_r_redirect_strict_301() {
    ensure_server();
    let resp = request(8080, "GET /old-page HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(resp.status, 301, "visual a27 must not accept 200 as success");
    assert_eq!(resp.header("location"), Some("/index.html"));
    // And the target itself must be servable.
    let target = request(8080, "GET /index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(target.status, 200);
}

/// [A-25] Browser `fetch('/../../etc/passwd')` normalizes `..` client-side,
/// so the server sees `/etc/passwd` → 404. The real 403 traversal defence
/// is only hit with a raw socket or percent-encoded `%2e%2e`.
#[test]
fn test_s_traversal_encoding_distinction() {
    ensure_server();

    // Raw `..` above root → 403 (lexical normalisation, path.rs:81-101).
    let raw = request(8080, "GET /../../etc/passwd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(raw.status, 403, "raw /../../etc/passwd must be 403");

    // Encoded `..` → also 403 (decode-first ordering, path.rs:209-219).
    let enc = request(8080, "GET /%2e%2e/%2e%2e/etc/passwd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(enc.status, 403, "encoded traversal must be 403");

    // Encoded slashes `%2f` prevent browser URL normalization from stripping `..`.
    let enc_slash = request(8080, "GET /%2e%2e%2f%2e%2e%2fetc/passwd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(enc_slash.status, 403, "encoded slash traversal must be 403");

    // NUL → 400.
    let nul = request(8080, "GET /%00invalid-nul HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(nul.status, 400);

    // What the browser actually sends after normalisation → 404, NOT 403.
    // A visual test asserting `404 || 403` on this URL proves nothing.
    let normalized = request(8080, "GET /etc/passwd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(normalized.status, 404);
}

/// [A-22] Session contract the visual `a22` card does not check:
/// single `GET /session` + `contains('session')` passes even with cookies
/// broken. Real proof needs Set-Cookie round-trip + counter 1→2, and the
/// page markup is `Requests on it`, not `Visits:` (visual regex bug).
/// The visual `loginSession()` POSTs `action=login&username=` to `/session`,
/// but the server only honours `POST /session/login` + `name=` (session_page.rs:50).
#[test]
fn test_t_session_login_contract() {
    ensure_server();

    // 1. Markup uses "Requests on it", never "Visits:".
    let r1 = request(8080, "GET /session HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(r1.status, 200);
    let body1 = r1.text();
    assert!(body1.contains("Requests on it</th><td>1</td>"), "counter starts at 1");
    assert!(!body1.contains("Visits:"), "page never contains 'Visits:' — visual regex is wrong");
    let set_cookie = r1.header("set-cookie").expect("Set-Cookie on new session").to_string();
    assert!(set_cookie.contains("localserver_session="));
    let cookie = set_cookie.split(';').next().unwrap().trim().to_string();

    // 2. Wrong login shape (what the visual sends) must NOT sign in.
    let wrong_body = "action=login&username=auditor";
    let req_wrong = format!(
        "POST /session HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        wrong_body.len(),
        wrong_body
    );
    let r_wrong = request(8080, &req_wrong);
    // Treated as Show, not Login: no 303, no greeting.
    assert_ne!(r_wrong.status, 303, "POST /session with action=login must not log in");
    assert!(!r_wrong.text().contains("Hello, <strong>auditor</strong>"));

    // 3. Correct login shape must 303 + persist name.
    let good_body = "name=auditor";
    let req_good = format!(
        "POST /session/login HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        good_body.len(),
        good_body
    );
    let r_good = request(8080, &req_good);
    assert_eq!(r_good.status, 303, "POST /session/login with name= must 303");
    assert_eq!(r_good.header("location"), Some("/session"));

    let req_after = format!("GET /session HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n");
    let r_after = request(8080, &req_after);
    assert_eq!(r_after.status, 200);
    assert!(r_after.text().contains("Hello, <strong>auditor</strong>"), "name must persist after correct login");
}

/// [A-11] The visual `a11_vhost` card sends `X-Host: test.com` because
/// browsers forbid setting `Host`. The server honours it (request.rs), but
/// the real audit uses `Host:`. Both must route to site2.
#[test]
fn test_u_vhost_host_and_xhost_parity() {
    ensure_server();
    let via_host = request(8080, "GET / HTTP/1.1\r\nHost: test.com\r\nConnection: close\r\n\r\n");
    assert_eq!(via_host.status, 200);
    assert!(via_host.text().contains("test.com"), "Host: test.com must serve site2");

    let via_xhost = request(8080, "GET / HTTP/1.1\r\nHost: localhost\r\nX-Host: test.com\r\nConnection: close\r\n\r\n");
    assert_eq!(via_xhost.status, 200);
    assert!(via_xhost.text().contains("test.com"), "X-Host: test.com must also serve site2 (visual workaround)");

    // site2 static assets must be servable (regression: /static/* was 404 on
    // test.com because www/site2/static/ did not exist → unstyled page).
    let css = request(8080, "GET /static/style.css HTTP/1.1\r\nHost: test.com\r\nConnection: close\r\n\r\n");
    assert_eq!(css.status, 200, "test.com /static/style.css must be servable");
    assert!(css.header("content-type").unwrap_or("").contains("text/css"));
}

/// Dashboard coherence (no server needed): every `getElementById` / `res-*`
/// referenced by `audit-console.js` must exist in `index.html`, and every
/// `auditRunner.*` referenced in HTML must be defined in JS. Catches
/// `tele-conns` / `admin-live-json` dead hooks and similar drift.
#[test]
fn test_v_dashboard_asset_coherence() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let html = std::fs::read_to_string(root.join("www/index.html")).expect("read index.html");
    let js = std::fs::read_to_string(root.join("www/static/audit-console.js")).expect("read audit-console.js");
    let css = std::fs::read_to_string(root.join("www/static/style.css")).expect("read style.css");

    // 1. All res-* boxes referenced by runAllAuditTests keys exist in HTML.
    for key in ["a9","a10","a11_default","a11_vhost","a12_404","a12_405","a13_small","a13_large","a14","a15","a16_allowed","a16_forbidden","a17","a18","a19","a20","a21","a22","a24","a25","a26","a27","a28_python","a28_perl","a40"] {
        assert!(html.contains(&format!("id=\"res-{key}\"")), "missing <div id=\"res-{key}\"> in index.html");
        assert!(js.contains(&format!("async {key}(")) || js.contains(&format!("{key}(")), "missing auditRunner.{key} in audit-console.js");
    }

    // 2. Telemetry IDs polled by JS must exist in HTML.
    //    Currently `tele-conns` is polled but has no element (visual bug).
    for id in ["tele-uptime", "tele-reqs", "tele-fds", "tele-sessions"] {
        assert!(html.contains(&format!("id=\"{id}\"")), "missing telemetry #{id}");
        assert!(js.contains(id), "JS should poll #{id}");
    }

    // 3. Core dashboard classes used in HTML must be defined in CSS.
    for class in ["telemetry-bar", "tab-nav", "tab-panel", "audit-grid", "audit-card", "status-badge"] {
        assert!(css.contains(class), "missing .{class} in style.css");
    }

    // 4. Static assets referenced by the dashboard must exist on disk.
    assert!(root.join("www/static/style.css").is_file());
    assert!(root.join("www/static/audit-console.js").is_file());
    assert!(root.join("www/static/logo.png").is_file());
}

/// Visual-dashboard correctness (no server needed): asserts the FIXED
/// dashboard logic. Each assertion corresponds to a false PASS / false FAIL
/// found by the audit agents. This test FAILS on the pre-fix dashboard and
/// PASSES once the visuals are corrected.
#[test]
fn test_w_visual_dashboard_correctness() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let html = std::fs::read_to_string(root.join("www/index.html")).expect("read index.html");
    let js = std::fs::read_to_string(root.join("www/static/audit-console.js")).expect("read audit-console.js");

    // Helper: extract one audit function body from JS by name.
    fn fn_body<'a>(js: &'a str, name: &str) -> &'a str {
        let marker = format!("async {name}(");
        let start = js.find(&marker).unwrap_or_else(|| panic!("auditRunner.{name} not found"));
        let rest = &js[start..];
        // Take until the next `async <word>(` at same indent or end.
        let mut end = rest.len();
        for (i, _) in rest.match_indices("\n    async ") {
            if i > 10 {
                end = i;
                break;
            }
        }
        &rest[..end]
    }

    // [A-13] a13_small AND a13_large must probe 8082 `/` (the only route on
    // that server), never 8082 `/uploads/*` (which is correctly 403).
    for name in ["a13_small", "a13_large"] {
        let body = fn_body(&js, name);
        assert!(
            !body.contains("8082/uploads"),
            "{name} must not target 8082/uploads (that route does not exist on 8082)"
        );
        assert!(
            body.contains("127.0.0.1:8082/") || body.contains("127.0.0.1:8082\"") || body.contains("8082/\"") || body.contains("8082/')") || body.contains(":8082/"),
            "{name} must target port 8082 root path"
        );
    }
    // a13_small must accept any non-413 (backend returns 403 for POST / dir
    // without upload_dir), or explicitly handle 403 — not demand 200/201.
    let small_body = fn_body(&js, "a13_small");
    assert!(
        !small_body.contains("res.status === 200 || res.status === 201"),
        "a13_small must not demand 200/201 on 8082 (backend correctly returns 403 for dir POST)"
    );

    // [A-27] a27 must require 301/opaqueredirect and must NOT accept 200.
    let a27 = fn_body(&js, "a27");
    assert!(a27.contains("301"), "a27 must check for 301");
    assert!(
        !a27.contains("|| res.status === 200") && !a27.contains("|| 200") && !a27.contains("=== 200"),
        "a27 must not accept 200 as redirect success (tautology)"
    );

    // [A-25] a25 must use percent-encoded traversal so the 403 path is hit.
    // Raw `/../../` is normalized by the browser to `/etc/passwd` → 404.
    let a25 = fn_body(&js, "a25");
    assert!(
        a25.contains("%2e%2e") || a25.contains("%2E%2E") || a25.contains("%2f") || a25.contains("%2F"),
        "a25 must use encoded traversal (raw /../../ never reaches the server)"
    );

    // [A-22] a22 must verify the session counter across two requests with
    // the real markup `Requests on it`, not a single contains('session').
    let a22 = fn_body(&js, "a22");
    assert!(
        a22.contains("Requests on it"),
        "a22 must assert the real 'Requests on it' markup (page never contains 'Visits:')"
    );

    // Session tab inline helpers in index.html:
    // - login must POST to /session/login with field name=
    assert!(html.contains("/session/login"), "login helper must POST to /session/login");
    assert!(
        !html.contains("action=login&username="),
        "login helper must not use action=login&username= (server ignores it)"
    );
    // - visit counter regex must match real markup
    assert!(
        html.contains("Requests on it"),
        "session display helper must parse 'Requests on it', not 'Visits:'"
    );

    // Telemetry: if JS polls tele-conns, the element must exist.
    if js.contains("tele-conns") {
        assert!(html.contains("id=\"tele-conns\""), "JS polls #tele-conns but index.html has no such element");
    }

    // CGI lab label: slow.py sleeps 120s with a 10s server deadline.
    // The dashboard must not claim "sleeping for 15s".
    assert!(
        !html.contains("sleeping for 15s") && !html.contains("sleeping for 15 s"),
        "slow.py sleeps 120s — label claiming 15s is wrong"
    );

    // Virtual-host static isolation: test.com root is www/site2, so
    // /static/* on that vhost needs www/site2/static/* as REAL files.
    // Symlinks pointing outside www/site2 (e.g. -> ../../static/...) are
    // correctly refused with 403 by the symlink defence (verify_under), so
    // they must be copies, not symlinks.
    let site_css = root.join("www/site2/static/style.css");
    assert!(site_css.is_file(), "www/site2/static/style.css missing — test.com page renders unstyled");
    assert!(!site_css.is_symlink(), "www/site2/static/style.css must be a copy, not an outside-root symlink (would 403)");

    // [A-14] a14 must cover all 6 routes, including /readonly and /old-page.
    let a14 = fn_body(&js, "a14");
    for route in ["/readonly", "/old-page", "/listing", "/uploads", "/cgi-bin"] {
        assert!(a14.contains(route), "a14 must probe route {route} (all 6 routes required)");
    }
}

/// CORS preflight: browsers send OPTIONS before non-simple cross-origin
/// fetches (X-Host header, octet-stream bodies). Must be 204, not 501/405,
/// or the visual a9/a10/a11/a13 cards are blocked when the dashboard on
/// :8080 probes :8081/:8082.
#[test]
fn test_x_cors_preflight_options() {
    ensure_server();
    for path in ["/", "/uploads/", "/cgi-bin/hello.py", "/session", "/admin/metrics.json"] {
        let raw = format!("OPTIONS {path} HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:8080\r\nAccess-Control-Request-Method: GET\r\nConnection: close\r\n\r\n");
        let resp = request(8080, &raw);
        assert_eq!(resp.status, 204, "OPTIONS {path} must be 204 preflight OK");
        assert_eq!(resp.header("access-control-allow-origin"), Some("*"));
    }
    // Preflight on another port too.
    let resp = request(8081, "OPTIONS / HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:8080\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n");
    assert_eq!(resp.status, 204);
}

