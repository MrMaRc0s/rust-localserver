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
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static SERVER: OnceLock<Mutex<ServerProcess>> = OnceLock::new();

fn ensure_server() {
    SERVER.get_or_init(|| {
        let bin = env!("CARGO_BIN_EXE_localserver");

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

        Mutex::new(ServerProcess { child })
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
