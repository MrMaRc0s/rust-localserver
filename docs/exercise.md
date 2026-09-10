## LocalServer

### Overview

Finally, you are going to understand how the internet works from the server side. The Hypertext Transfer Protocol was created in order to ensure a reliable way to communicate on a request/response basis.

This protocol is used by servers and clients (usually browsers) to serve content, and it is the backbone of the World Wide Web. Still, it is also used in many other cases that are far beyond the scope of this exercise.

For this project, you **must** use **Rust**.

### Role Play

You are a backend engineer at a startup building a lightweight web server to handle internal APIs and static content with minimal dependencies. Your goal is to deliver a highly available, crash-proof solution that can be extended to support dynamic content via CGI scripts and configured to suit multiple environments.

### Learning Objective

By the end of this project, learners will be able to:

- Design and implement a custom HTTP/1.1-compliant server in Rust
- Utilize non-blocking I/O mechanisms
- Parse and construct HTTP requests and responses manually
- Configure server routes, error pages, uploads, and CGI scripts
- Evaluate performance under stress and ensure memory and process safety

Technical skills:

- Socket programming
- Asynchronous I/O
- File and process management
- Configuration parsing

### Instructions

- The project must be written in **Rust**.
- Use Rust’s standard library together with the `mio` crate (or similar) for non-blocking I/O.
- Make use of an event-driven API for handling connections.

> You **cannot** use established server frameworks or asynchronous runtimes (e.g., `axum`, `rocket`, `hyper`, `async-std`, `tokio`).

#### The Server

Your goal is to write your own HTTP server to serve static web pages to browsers.

It must:

- **Never** crash.
- Timeout long requests.
- Listen on multiple ports and instantiate multiple servers.
- Use only one process and one thread.
- Receive requests and send HTTP/1.1-compliant responses.
- Handle `GET`, `POST`, and `DELETE`.
- Receive file uploads.
- Handle cookies and sessions.
- Provide default error pages for: 400, 403, 404, 405, 413, 500.
- Use an event-driven, non-blocking I/O API.
- Manage chunked and unchunked requests.
- Set the correct HTTP status in responses.

#### The CGI

- Execute one type of CGI (e.g., `.py`) using Rust’s `std::process::Command`.
- Pass the file to process as the first argument.
- Use the `PATH_INFO` environment variable to define full paths.
- Ensure correct relative path handling.

#### Configuration File

Support configuration for:

- Host and multiple ports.
- Default server selection.
- Custom error page paths.
- Client body size limit.
- Routes with:
  - Accepted methods.
  - Redirections.
  - Directory/file roots.
  - Default file for directories.
  - CGI by file extension.
  - Directory listing toggle.
  - Default directory response file.

> No need for regex support.

#### Testing

- Use `siege -b [IP]:[PORT]` for stress testing (target 99.5% availability).
- Write comprehensive tests (redirections, configs, error pages, etc.).
- Check for memory leaks (Rust guarantees memory safety, but check file descriptor leaks).

#### Bonus Challenges

- Implement a second CGI handler.
- Create an admin dashboard or server metrics endpoint.

### Unit Tests

You must implement unit tests within your `LocalServer` project to ensure your protocol parsing and configuration logic are robust. Specifically, your tests should:

- Verify **HTTP Request Parsing** by ensuring headers, methods (GET, POST, DELETE), and body content (including chunked encoding) are correctly extracted from raw byte streams.
- Verify **Configuration Validation** by ensuring the server correctly identifies conflicting ports, invalid paths, and out-of-bounds body size limits during startup.
- Test **Route Matching** logic to confirm that requests are mapped to the correct directory roots, CGI scripts, or redirections based on the configuration file.
- Verify **Status Code Generation** by ensuring the internal logic returns the correct `4xx` or `5xx` codes for malformed requests or missing files before the response is sent.

### Example Repository Structure

```
/rust-server
├── /src
│   ├── main.rs           # Entry point
│   ├── server.rs         # Handles server lifecycle
│   ├── router.rs         # Routes requests
│   ├── cgi.rs            # Manages CGI execution
│   ├── config.rs         # Parses configuration file
│   ├── error.rs          # Error responses
│   ├── utils/
│       ├── session.rs    # Session management
│       ├── cookie.rs     # Cookie utilities
├── config.yaml           # Server config
├── README.md             # Project overview and setup
├── error_pages/          # Static error page files
```

> **Note:** Simplified illustration — the canonical layout is defined in `TASKS.md` Phase 0 Task 0.1. Actual `src/` splits `config.rs` into `config/{mod,parser,validate}.rs`, expands `http/` to `{mod,request,chunked,multipart,response}.rs` (`multipart` — Phase 3.3), `handlers/` to `{mod,session_page}.rs` (`session_page` — Phase 6.2), and `utils/` to `{mod,session,cookie,mime,path}.rs`; `src/connection.rs` is also present. `Cargo.toml` enables `mio` with `os-poll`, `net`, `os-ext` (`os-ext` for `SourceFd` CGI pipes).

### Tips

- Avoid hardcoding; use the config file.
- Validate configs at startup.
- Sanitize inputs for CGI.
- Modularize components.
- Use thread-safe data structures.
- Prevent file descriptor leaks.

### Resources

- [RFC 2616 – HTTP/1.1 Specification](https://www.rfc-editor.org/rfc/rfc9112.html)
- [mio Docs](https://docs.rs/crate/mio/latest)
- [epoll Example](https://man7.org/linux/man-pages/man7/epoll.7.html)
- [CGI Protocol Overview](https://en.wikipedia.org/wiki/Common_Gateway_Interface)
- [siege Load Testing Tool](https://github.com/JoeDog/siege)

### Disclaimer

This project is for educational use only. Using siege or any stress testing tool against a third-party server without explicit permission is illegal and unethical.
