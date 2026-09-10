//! CGI/1.1 execution, driven entirely by the event loop.
//!
//! # Why this is the hardest part to get right ([A-8])
//!
//! The obvious implementation is `Command::output()` or
//! `child.wait_with_output()`. Both **block until the script exits**, and with
//! one thread that means every other client waits too: a script that sleeps for
//! five seconds freezes the whole server for five seconds. It also fails audit
//! question [A-8] outright, which asks whether *all* reading and writing goes
//! through epoll — a pipe is a file descriptor, and reading it is reading.
//!
//! So the child's three streams are set `O_NONBLOCK`, wrapped in
//! `mio::unix::SourceFd`, and registered in **the same `Poll`** as every socket.
//! The request body is pumped into stdin one write per writable event, stdout is
//! collected one read per readable event, and the connection that is waiting
//! goes back to sleep in between like any other. The server keeps serving.
//!
//! # Reaping and killing
//!
//! `Child::try_wait` is `waitpid(WNOHANG)` — it reports whether the process has
//! exited without waiting for it to. Called from the loop tick, it keeps
//! finished scripts from lingering as zombies, which would show up in the audit
//! as a process leak.
//!
//! A script that never exits is bounded by [`CGI_TIMEOUT`]: the child is killed,
//! its pipes are closed and the client gets 504. Without that, one infinite loop
//! is a permanent process and three permanent file descriptors.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use mio::Token;

use crate::config::ServerConfig;
use crate::http::request::Request;
use crate::http::response::Response;

/// How long a script may run before it is killed.
///
/// Shorter than the connection idle timeout (30 s) on purpose, so a runaway
/// script is reported as a gateway timeout rather than silently becoming a
/// stalled connection.
pub const CGI_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on what a script may write.
///
/// Output is buffered so that `Content-Length` can be computed exactly, which
/// keeps `HEAD` honest and avoids chunking a response whose size is already
/// known. The cap is what makes buffering safe: a script printing an endless
/// stream is stopped rather than allowed to exhaust memory.
pub const MAX_CGI_OUTPUT: usize = 1024 * 1024;

/// Bytes moved per pipe event.
const PIPE_CHUNK: usize = 32 * 1024;

/// Cap on captured stderr, which is only ever logged.
const MAX_CGI_STDERR: usize = 16 * 1024;

/// Everything needed to start a script, gathered while routing.
#[derive(Debug, Clone)]
pub struct CgiSpec {
    pub interpreter: PathBuf,
    pub script: PathBuf,
    /// RFC 3875 `PATH_INFO`: the URL portion after the script name.
    pub path_info: String,
    pub query: String,
    pub method: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub server_name: String,
    pub server_port: u16,
    pub script_name: String,
    pub document_root: PathBuf,
    pub remote: SocketAddr,
    pub protocol: String,
    pub head_only: bool,
}

impl CgiSpec {
    /// Build the environment for the child, per RFC 3875.
    ///
    /// The environment is **cleared first**: a script should see the request,
    /// not whatever happened to be exported in the shell that launched the
    /// server. `PATH` is put back because an interpreter generally needs one.
    pub fn environment(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = Vec::with_capacity(self.headers.len() + 20);
        let mut set = |key: &str, value: String| env.push((key.to_string(), value));

        set("GATEWAY_INTERFACE", "CGI/1.1".to_string());
        set("SERVER_SOFTWARE", "localserver".to_string());
        set("SERVER_PROTOCOL", self.protocol.clone());
        set("SERVER_NAME", self.server_name.clone());
        set("SERVER_PORT", self.server_port.to_string());
        set("REQUEST_METHOD", self.method.clone());
        set("QUERY_STRING", self.query.clone());
        set("SCRIPT_NAME", self.script_name.clone());
        // The full filesystem path of the script. This, and PATH_TRANSLATED
        // below, are the "full paths" the subject asks for; PATH_INFO itself is
        // the *extra* path after the script name, as RFC 3875 defines it.
        set("SCRIPT_FILENAME", self.script.display().to_string());
        set("PATH_INFO", self.path_info.clone());
        if !self.path_info.is_empty() {
            // PATH_INFO mapped onto the filesystem, which is what a script that
            // wants a real path should read.
            let translated = self
                .document_root
                .join(self.path_info.trim_start_matches('/'));
            set("PATH_TRANSLATED", translated.display().to_string());
        }
        set("DOCUMENT_ROOT", self.document_root.display().to_string());
        set("REMOTE_ADDR", self.remote.ip().to_string());
        set("REMOTE_PORT", self.remote.port().to_string());
        // Some CGI runtimes refuse to start without this.
        set("REDIRECT_STATUS", "200".to_string());
        set("PATH", "/usr/local/bin:/usr/bin:/bin".to_string());

        if !self.body.is_empty() {
            set("CONTENT_LENGTH", self.body.len().to_string());
        }

        for (name, value) in &self.headers {
            // RFC 3875: these two travel as CONTENT_* and not as HTTP_*.
            if name == "content-type" {
                set("CONTENT_TYPE", value.clone());
                continue;
            }
            if name == "content-length" {
                continue;
            }
            let mangled: String = name
                .chars()
                .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
                .collect();
            set(&format!("HTTP_{mangled}"), value.clone());
        }

        env
    }
}

/// Where the script lives and how it was reached, as the router determined.
pub struct CgiTarget {
    pub interpreter: PathBuf,
    pub script: PathBuf,
    pub path_info: String,
    pub document_root: PathBuf,
}

/// Where the request came from, as the connection knows it.
pub struct CgiOrigin {
    pub remote: SocketAddr,
    pub port: u16,
    pub head_only: bool,
}

/// Gather everything a script needs from a routed request.
pub fn spec_from_request(
    target: CgiTarget,
    origin: CgiOrigin,
    request: &Request,
    server: &ServerConfig,
) -> CgiSpec {
    let CgiTarget { interpreter, script, path_info, document_root } = target;
    let CgiOrigin { remote, port, head_only } = origin;

    // Absolute paths from here on. The child runs with its working directory
    // set to the script's own directory, so a relative script path would be
    // resolved against that directory a second time — `cgi-bin/hello.py` would
    // become `cgi-bin/cgi-bin/hello.py`. Config paths are relative to the
    // config file, so this is the norm rather than the exception.
    let script = script.canonicalize().unwrap_or(script);
    let document_root = document_root.canonicalize().unwrap_or(document_root);
    let script_name = match request.path.strip_suffix(&path_info) {
        Some(name) if !path_info.is_empty() => name.to_string(),
        _ => request.path.clone(),
    };
    CgiSpec {
        interpreter,
        script,
        path_info,
        query: request.query.clone(),
        method: request.method.clone(),
        body: request.body.clone(),
        headers: request
            .headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        server_name: request
            .host()
            .unwrap_or_else(|| server.host.clone()),
        server_port: port,
        script_name,
        document_root,
        remote,
        protocol: request.version.as_str().to_string(),
        head_only,
    }
}

/// What a pump step concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgiStep {
    /// More to do; wait for the next event.
    Continue,
    /// This stream reached end of file.
    Closed,
    /// Fatal for this script; the `u16` is the status to answer with.
    Failed(u16),
}

/// A running script and its three pipes.
pub struct CgiProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    /// Tokens the pipes are registered under, so they can be deregistered.
    pub stdin_token: Token,
    pub stdout_token: Token,
    pub stderr_token: Token,
    input: Vec<u8>,
    input_pos: usize,
    output: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_closed: bool,
    stderr_closed: bool,
    deadline: Instant,
    pub head_only: bool,
    /// Set once the process is being torn down, so it is not killed twice.
    finished: bool,
}

impl CgiProcess {
    /// Start a script with all three streams piped and non-blocking.
    ///
    /// `spec.script` must be absolute: the working directory is changed to the
    /// script's own directory so that a relative `open("data.txt")` inside the
    /// script resolves next to it, which also means a relative script path
    /// would be resolved against that new directory.
    pub fn spawn(
        spec: &CgiSpec,
        stdin_token: Token,
        stdout_token: Token,
        stderr_token: Token,
    ) -> io::Result<CgiProcess> {
        let working_dir = spec
            .script
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let mut command = Command::new(&spec.interpreter);
        command
            .arg(&spec.script)
            // A script doing `open("data.txt")` must resolve it next to itself,
            // not next to wherever the server happened to be started.
            .current_dir(&working_dir)
            .env_clear()
            .envs(spec.environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Without O_NONBLOCK a read on an idle pipe blocks the whole server,
        // which is the entire failure this module exists to avoid.
        for fd in [
            stdin.as_ref().map(|s| s.as_raw_fd()),
            stdout.as_ref().map(|s| s.as_raw_fd()),
            stderr.as_ref().map(|s| s.as_raw_fd()),
        ]
        .into_iter()
        .flatten()
        {
            set_nonblocking(fd)?;
        }

        Ok(CgiProcess {
            child,
            stdin,
            stdout,
            stderr,
            stdin_token,
            stdout_token,
            stderr_token,
            input: spec.body.clone(),
            input_pos: 0,
            output: Vec::new(),
            stderr_buf: Vec::new(),
            stdout_closed: false,
            stderr_closed: false,
            deadline: Instant::now() + CGI_TIMEOUT,
            head_only: spec.head_only,
            finished: false,
        })
    }

    pub fn stdin_fd(&self) -> Option<RawFd> {
        self.stdin.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn stdout_fd(&self) -> Option<RawFd> {
        self.stdout.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn stderr_fd(&self) -> Option<RawFd> {
        self.stderr.as_ref().map(|s| s.as_raw_fd())
    }

    /// Is there a body still to hand the script?
    pub fn has_input(&self) -> bool {
        self.stdin.is_some() && self.input_pos < self.input.len()
    }

    /// One write into the script's stdin.
    pub fn on_stdin_writable(&mut self) -> CgiStep {
        let stdin = match self.stdin.as_mut() {
            Some(stdin) => stdin,
            None => return CgiStep::Closed,
        };
        let pending = &self.input[self.input_pos..];
        if pending.is_empty() {
            return CgiStep::Closed;
        }
        let chunk = &pending[..PIPE_CHUNK.min(pending.len())];
        match stdin.write(chunk) {
            Ok(0) => CgiStep::Closed,
            Ok(n) => {
                self.input_pos += n;
                if self.input_pos >= self.input.len() {
                    CgiStep::Closed
                } else {
                    CgiStep::Continue
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => CgiStep::Continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => CgiStep::Continue,
            // A script that exits without reading its body gives EPIPE. That is
            // perfectly legal — plenty of scripts ignore stdin — so it closes
            // the stream rather than failing the request.
            Err(_) => CgiStep::Closed,
        }
    }

    /// One read from the script's stdout.
    pub fn on_stdout_readable(&mut self) -> CgiStep {
        let mut chunk = [0u8; PIPE_CHUNK];
        let stdout = match self.stdout.as_mut() {
            Some(stdout) => stdout,
            None => return CgiStep::Closed,
        };
        match stdout.read(&mut chunk) {
            Ok(0) => {
                self.stdout_closed = true;
                CgiStep::Closed
            }
            Ok(n) => {
                if self.output.len() + n > MAX_CGI_OUTPUT {
                    return CgiStep::Failed(502);
                }
                self.output.extend_from_slice(&chunk[..n]);
                CgiStep::Continue
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => CgiStep::Continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => CgiStep::Continue,
            Err(_) => {
                self.stdout_closed = true;
                CgiStep::Closed
            }
        }
    }

    /// One read from the script's stderr, which is logged rather than sent.
    pub fn on_stderr_readable(&mut self) -> CgiStep {
        let mut chunk = [0u8; PIPE_CHUNK];
        let stderr = match self.stderr.as_mut() {
            Some(stderr) => stderr,
            None => return CgiStep::Closed,
        };
        match stderr.read(&mut chunk) {
            Ok(0) => {
                self.stderr_closed = true;
                CgiStep::Closed
            }
            Ok(n) => {
                let room = MAX_CGI_STDERR.saturating_sub(self.stderr_buf.len());
                if room > 0 {
                    self.stderr_buf.extend_from_slice(&chunk[..n.min(room)]);
                }
                CgiStep::Continue
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => CgiStep::Continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => CgiStep::Continue,
            Err(_) => {
                self.stderr_closed = true;
                CgiStep::Closed
            }
        }
    }

    /// Stop feeding the script: closes its stdin so a program reading to EOF
    /// can proceed.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn close_stdout(&mut self) {
        self.stdout = None;
        self.stdout_closed = true;
    }

    pub fn close_stderr(&mut self) {
        self.stderr = None;
        self.stderr_closed = true;
    }

    /// Both output streams reached EOF: the script has said everything it will.
    pub fn output_complete(&self) -> bool {
        self.stdout_closed && self.stderr_closed
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        !self.finished && now >= self.deadline
    }

    /// `waitpid(WNOHANG)`: has it exited, without waiting for it to.
    pub fn reap(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Kill and reap, for a script that overran its deadline.
    pub fn kill(&mut self) {
        self.finished = true;
        let _ = self.child.kill();
        // Reap immediately so the killed child does not linger as a zombie.
        let _ = self.child.wait();
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr_buf).trim_end().to_string()
    }

    /// Turn what the script wrote into an HTTP response.
    pub fn into_response(self) -> Response {
        translate(&self.output, self.head_only)
    }
}

impl Drop for CgiProcess {
    /// A connection dropped mid-script must not leave the process running.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is owned by a live ChildStdin/Stdout/Stderr for the duration
    // of this call, and both calls are the documented fcntl contract.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Parse a CGI script's output into a response.
///
/// A script writes its own header block — `Status:`, `Content-Type:`,
/// `Location:` and anything else — then a blank line, then the body. `Status:`
/// is CGI's way of setting the response code and is *not* forwarded as a header.
pub fn translate(output: &[u8], head_only: bool) -> Response {
    let (head, body) = match split_head(output) {
        Some(split) => split,
        // No header block at all. Rather than guess, treat the whole thing as a
        // plain-text body: a script that forgot its headers still shows the
        // author what it printed instead of a bare 502.
        None => {
            return Response::with_body(200, "text/plain; charset=utf-8", output.to_vec())
                .head_only(head_only)
        }
    };

    let text = String::from_utf8_lossy(head);
    let mut status = 200u16;
    let mut headers: Vec<(String, String)> = Vec::new();

    for line in text.split("\r\n").flat_map(|l| l.split('\n')) {
        if line.trim().is_empty() {
            continue;
        }
        let (name, value) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            // "302 Found" or just "302".
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
            continue;
        }
        // A script must not be able to decide how the connection is framed:
        // those belong to the server, and honouring them would let a script
        // desynchronise the byte stream for the next request.
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        headers.push((name.to_string(), value.to_string()));
    }

    // A Location with no explicit Status is a redirect, per RFC 3875.
    if status == 200 && headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("location")) {
        status = 302;
    }

    let mut response = Response::new(status);
    for (name, value) in headers {
        response = response.header(&name, value);
    }
    if !response.has_header("content-type") {
        response = response.header("Content-Type", "text/plain; charset=utf-8");
    }
    // The body is fully buffered, so the length is known exactly — no need to
    // chunk a response whose size is not in doubt, and HEAD stays honest.
    response.body(body.to_vec()).head_only(head_only)
}

/// Split a CGI response into its header block and body.
fn split_head(output: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(i) = output.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((&output[..i], &output[i + 4..]));
    }
    if let Some(i) = output.windows(2).position(|w| w == b"\n\n") {
        return Some((&output[..i], &output[i + 2..]));
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::Path;

    fn spec() -> CgiSpec {
        CgiSpec {
            interpreter: PathBuf::from("/usr/bin/python3"),
            script: PathBuf::from("/srv/cgi-bin/app.py"),
            path_info: "/users/7".to_string(),
            query: "a=1&b=2".to_string(),
            method: "POST".to_string(),
            body: b"payload".to_vec(),
            headers: vec![
                ("host".to_string(), "example.com".to_string()),
                ("content-type".to_string(), "text/plain".to_string()),
                ("content-length".to_string(), "7".to_string()),
                ("x-custom-header".to_string(), "value".to_string()),
            ],
            server_name: "example.com".to_string(),
            server_port: 8080,
            script_name: "/cgi-bin/app.py".to_string(),
            document_root: PathBuf::from("/srv/cgi-bin"),
            remote: "127.0.0.1:54321".parse().unwrap(),
            protocol: "HTTP/1.1".to_string(),
            head_only: false,
        }
    }

    fn env_of(spec: &CgiSpec) -> std::collections::HashMap<String, String> {
        spec.environment().into_iter().collect()
    }

    // --- environment ([A-28], and the subject's PATH_INFO requirement) -------

    #[test]
    fn sets_the_required_cgi_variables() {
        let env = env_of(&spec());
        assert_eq!(env.get("GATEWAY_INTERFACE").map(String::as_str), Some("CGI/1.1"));
        assert_eq!(env.get("REQUEST_METHOD").map(String::as_str), Some("POST"));
        assert_eq!(env.get("QUERY_STRING").map(String::as_str), Some("a=1&b=2"));
        assert_eq!(env.get("SERVER_PROTOCOL").map(String::as_str), Some("HTTP/1.1"));
        assert_eq!(env.get("SERVER_NAME").map(String::as_str), Some("example.com"));
        assert_eq!(env.get("SERVER_PORT").map(String::as_str), Some("8080"));
        assert_eq!(env.get("REMOTE_ADDR").map(String::as_str), Some("127.0.0.1"));
        assert_eq!(env.get("SCRIPT_NAME").map(String::as_str), Some("/cgi-bin/app.py"));
        assert_eq!(env.get("CONTENT_LENGTH").map(String::as_str), Some("7"));
        assert_eq!(env.get("CONTENT_TYPE").map(String::as_str), Some("text/plain"));
    }

    /// `PATH_INFO` is the extra path after the script, per RFC 3875; the *full
    /// filesystem paths* the subject asks about are `SCRIPT_FILENAME` and
    /// `PATH_TRANSLATED`.
    #[test]
    fn path_info_and_the_full_paths_are_distinct() {
        let env = env_of(&spec());
        assert_eq!(env.get("PATH_INFO").map(String::as_str), Some("/users/7"));
        assert_eq!(
            env.get("SCRIPT_FILENAME").map(String::as_str),
            Some("/srv/cgi-bin/app.py")
        );
        assert_eq!(
            env.get("PATH_TRANSLATED").map(String::as_str),
            Some("/srv/cgi-bin/users/7")
        );
    }

    #[test]
    fn no_path_translated_without_path_info() {
        let mut spec = spec();
        spec.path_info = String::new();
        let env = env_of(&spec);
        assert_eq!(env.get("PATH_INFO").map(String::as_str), Some(""));
        assert!(!env.contains_key("PATH_TRANSLATED"));
    }

    /// Request headers reach the script as `HTTP_*`, except the two that
    /// RFC 3875 gives dedicated names.
    #[test]
    fn headers_become_http_variables() {
        let env = env_of(&spec());
        assert_eq!(env.get("HTTP_HOST").map(String::as_str), Some("example.com"));
        assert_eq!(env.get("HTTP_X_CUSTOM_HEADER").map(String::as_str), Some("value"));
        assert!(!env.contains_key("HTTP_CONTENT_TYPE"), "travels as CONTENT_TYPE");
        assert!(!env.contains_key("HTTP_CONTENT_LENGTH"), "travels as CONTENT_LENGTH");
    }

    /// The bug this test exists for: the child's working directory is the
    /// script's own directory, so a relative script path gets resolved against
    /// it twice — `cgi-bin/hello.py` becomes `cgi-bin/cgi-bin/hello.py` and the
    /// interpreter reports "no such file".
    #[test]
    fn the_script_path_is_made_absolute() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let relative = PathBuf::from("cgi-bin");
        // Run from the repository root, as the server does.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(root).unwrap();

        let mut parser = crate::http::request::RequestParser::new(1024);
        parser.feed(b"GET /cgi-bin/hello.py HTTP/1.1\r\nHost: x\r\n\r\n");
        let request = parser.take().unwrap();

        let spec = spec_from_request(
            CgiTarget {
                interpreter: PathBuf::from("/usr/bin/python3"),
                script: relative.join("hello.py"),
                path_info: String::new(),
                document_root: relative,
            },
            CgiOrigin { remote: "127.0.0.1:1".parse().unwrap(), port: 8080, head_only: false },
            &request,
            &server_config(),
        );

        std::env::set_current_dir(previous).unwrap();
        assert!(spec.script.is_absolute(), "got {:?}", spec.script);
        assert!(spec.document_root.is_absolute(), "got {:?}", spec.document_root);
        assert!(spec.script.ends_with("cgi-bin/hello.py"));
    }

    fn server_config() -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".to_string(),
            ports: vec![8080],
            server_names: Vec::new(),
            is_default: true,
            client_max_body_size: 1024,
            error_pages: std::collections::BTreeMap::new(),
            routes: Vec::new(),
            line: 1,
        }
    }

    #[test]
    fn a_body_free_request_sets_no_content_length() {
        let mut spec = spec();
        spec.body.clear();
        spec.headers.retain(|(n, _)| n != "content-length");
        assert!(!env_of(&spec).contains_key("CONTENT_LENGTH"));
    }

    // --- response translation ([A-28], task 7.3) ----------------------------

    #[test]
    fn translates_a_normal_script_response() {
        let response = translate(b"Content-Type: text/html\r\n\r\n<h1>hi</h1>", false);
        assert_eq!(response.status, 200);
        assert_eq!(response.get_header("Content-Type"), Some("text/html"));
        assert_eq!(response.body, b"<h1>hi</h1>");
    }

    #[test]
    fn accepts_bare_lf_headers() {
        let response = translate(b"Content-Type: text/plain\n\nbody text", false);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"body text");
    }

    /// `Status:` is how CGI sets the response code, and it is not forwarded as
    /// a header of its own.
    #[test]
    fn a_status_header_sets_the_response_code() {
        let response = translate(b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\ngone", false);
        assert_eq!(response.status, 404);
        assert!(response.get_header("Status").is_none());

        let bare = translate(b"Status: 201\r\n\r\n", false);
        assert_eq!(bare.status, 201);
    }

    /// RFC 3875: a Location with no explicit Status is a redirect.
    #[test]
    fn a_location_without_a_status_becomes_a_redirect() {
        let response = translate(b"Location: /elsewhere\r\n\r\n", false);
        assert_eq!(response.status, 302);
        assert_eq!(response.get_header("Location"), Some("/elsewhere"));

        // An explicit status wins over the inference.
        let explicit = translate(b"Status: 301\r\nLocation: /perm\r\n\r\n", false);
        assert_eq!(explicit.status, 301);
    }

    /// A script must not get to decide how the connection is framed: honouring
    /// its `Content-Length` or `Connection` would let it desynchronise the byte
    /// stream for the next request on the same connection.
    #[test]
    fn framing_headers_from_the_script_are_ignored() {
        let response = translate(
            b"Content-Type: text/plain\r\nContent-Length: 999\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\nshort",
            false,
        );
        assert!(response.get_header("Connection").is_none());
        assert!(response.get_header("Transfer-Encoding").is_none());
        // The server computes the real length from what was actually produced.
        let wire = String::from_utf8_lossy(&response.to_bytes(true)).to_string();
        assert!(wire.contains("Content-Length: 5"), "{wire}");
        assert!(!wire.contains("999"));
    }

    #[test]
    fn custom_headers_are_forwarded() {
        let response = translate(b"Content-Type: text/plain\r\nX-Powered-By: python\r\n\r\nok", false);
        assert_eq!(response.get_header("X-Powered-By"), Some("python"));
    }

    #[test]
    fn a_missing_content_type_defaults_to_plain_text() {
        let response = translate(b"Status: 200\r\n\r\nbody", false);
        assert_eq!(
            response.get_header("Content-Type"),
            Some("text/plain; charset=utf-8")
        );
    }

    /// A script that forgot its header block should still show its author what
    /// it printed, rather than a bare 502 with the output discarded.
    #[test]
    fn output_without_a_header_block_is_served_as_plain_text() {
        let response = translate(b"just some output with no headers", false);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"just some output with no headers");
    }

    #[test]
    fn an_empty_body_is_fine() {
        let response = translate(b"Content-Type: text/plain\r\n\r\n", false);
        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
    }

    #[test]
    fn head_requests_drop_the_body_but_keep_the_length() {
        let response = translate(b"Content-Type: text/plain\r\n\r\n0123456789", true);
        let wire = String::from_utf8_lossy(&response.to_bytes(true)).to_string();
        assert!(wire.contains("Content-Length: 10"));
        assert!(wire.ends_with("\r\n\r\n"), "no body should follow");
    }

    #[test]
    fn splits_the_header_block_on_either_line_ending() {
        assert_eq!(split_head(b"A: 1\r\n\r\nbody"), Some((&b"A: 1"[..], &b"body"[..])));
        assert_eq!(split_head(b"A: 1\n\nbody"), Some((&b"A: 1"[..], &b"body"[..])));
        assert_eq!(split_head(b"no terminator"), None);
    }
}
