# LocalServer — Audit Quick Reference & Code Walkthrough Notes

This document is the merged, definitive code-pointer cheat sheet for auditing **LocalServer**. It combines:
1. **The Rehearsal Checklist**: Commands to run before handing your laptop to the auditor.
2. **The Browser Verification Checklist**: Steps to demonstrate live in Chrome/Firefox DevTools.
3. **Master 40-Question Reference Table**: Exact file and line citations for every question `[A-1]` to `[A-40]`.
4. **Detailed Categorized Walkthrough**: Source pointers, reproducers, and defense points grouped by category.

---

## 1. Rehearsal Checklist (Run Before Handing Laptop to Auditor)

```bash
# 1. Verify tests pass (262 passed: 246 unit + 16 integration)
cargo test

# 2. Verify clean linter (0 warnings)
cargo clippy --all-targets

# 3. Start server in background (binds 8080, 8081, 8082)
cargo run --release -- config.yaml &

# 4. Run automated audit curl walkthrough (20/20 PASS on A-9..A-28)
./tests/audit.sh

# 5. Run async stress harness (simulates siege -b -c 25 -t 10s)
# Asserts: >=99.5% availability, 0 FD leaks, 0 CLOSE_WAIT sockets, 0 zombies
python3 tests/stress.py

# 6. Verify deliberate misconfigurations
cargo run -- config.dup-port.yaml     # Must print fatal conflict error & exit 2 (A-30)
cargo run -- config.partial-bad.yaml  # Must warn on bad root, then serve site1 & site3 (A-31)

# 7. Verify bonus features
curl -i http://127.0.0.1:8080/admin           # HTML admin dashboard (A-40)
curl -i http://127.0.0.1:8080/admin/metrics.json # JSON telemetry API (A-40)
curl -i http://127.0.0.1:8080/cgi-bin/hello.py   # Python CGI (A-39)
curl -i http://127.0.0.1:8080/cgi-bin/info.pl    # Perl CGI (A-39 bonus)
```

---

## 2. Browser Verification Checklist (Questions A-23..A-28)
## 2. Browser Verification & Visual Audit Studio (http://127.0.0.1:8080/)

Open Google Chrome or Firefox and navigate to `http://127.0.0.1:8080/`:
1. **Network Panel Inspection ([`A-23`], [`A-24`])**:
   - Confirm status `200 OK`.
   - Inspect response headers: `Date` (IMF-fixdate), `Server: localserver/0.1.0`, `Content-Type: text/html; charset=utf-8`, `Content-Length`, `Connection: keep-alive`.
   - Check assets: `style.css` (`text/css`), `logo.png` (`image/png`).
2. **Custom 404 Error Page ([`A-12`], [`A-25`])**:
   - Navigate to `http://127.0.0.1:8080/nonexistent`.
   - Verify status `404 Not Found` rendering custom styled error body from `error_pages/404.html`.
3. **HTTP 301 Redirection ([`A-27`])**:
   - Navigate to `http://127.0.0.1:8080/old-page`.
   - Verify browser receives `301 Moved Permanently` with `Location: /index.html` and automatically follows to home page.
4. **Directory Autoindex ([`A-26`])**:
   - Navigate to `http://127.0.0.1:8080/listing/`.
   - Verify generated HTML directory listing shows files, file sizes, and clickable links.
5. **CGI Form Execution ([`A-18`], [`A-28`])**:
   - Navigate to `http://127.0.0.1:8080/cgi-bin/hello.py/user/42?lang=rust`.
   - Check rendered environment variables: `REQUEST_METHOD=GET`, `PATH_INFO=/user/42`, `QUERY_STRING=lang=rust`.
6. **Session & Cookie System ([`A-22`])**:
   - Navigate to `http://127.0.0.1:8080/session`.
   - Open DevTools → **Application** (Storage) → **Cookies** → `http://127.0.0.1:8080`.
   - Verify `session_id` cookie is present with `HttpOnly` and `SameSite=Lax`.
   - Reload the page: observe visit counter increment (`Visits: 1` → `Visits: 2`).
Open Google Chrome or Firefox and navigate to `http://127.0.0.1:8080/`. The site loads a complete **Interactive Visual Audit Dashboard** requiring **zero curl commands**:

1. **🚀 Visual Audit Suite (`tab-audit`)**:
   - Click **`▶ Run All Audit Tests (A-9 .. A-40)`** to run every compliance test sequentially.
   - Each card displays real-time `[PASS]` status badges, HTTP status codes (`200`, `204`, `301`, `400`, `404`, `405`, `413`, `504`), response latencies, formatted RFC 9110 response headers, and response bodies.
2. **📦 File Upload & SHA-256 Studio (`tab-upload`) ([`A-21`])**:
   - Drag and drop any binary file. The client pre-computes SHA-256 via the Web Crypto API (`crypto.subtle.digest`), uploads it via `multipart/form-data` to `/uploads/`, downloads it back, and visually compares both checksums to verify zero corruption.
3. **⚙️ CGI Execution Lab (`tab-cgi`) ([`A-18`], [`A-28`], [`A-39`])**:
   - Click to execute Python (`hello.py`), Perl (`info.pl`), and POST form scripts.
   - Click **`Timeout Test (slow.py)`** to observe non-blocking 504 Gateway Timeout handling without stalling the server.
4. **🍪 Session & Cookie Manager (`tab-session`) ([`A-22`])**:
   - Inspect active `localserver_session` cookie (`HttpOnly`, `SameSite=Lax`).
   - Click **`Increment Visit Counter`** to see visits increment in memory.
5. **🔀 Routes & Virtual Hosts Explorer (`tab-routes`) ([`A-11`], [`A-14`])**:
   - Interactive table of all 6 routes. Switch between `localhost` and `test.com` virtual hosts on port 8080.
6. **🛠️ HTTP Request Console (`tab-console`)**:
   - Custom in-browser HTTP client for arbitrary request crafting and raw header inspection.
7. **Live Telemetry Bar**:
   - Real-time polling of `/admin/metrics.json` updating status (online pulse), uptime, total requests, open file descriptors (`/proc/self/fd`), and active sessions.

*Alternative Direct Browser URLs for Manual Inspection:*
- Custom 404: `http://127.0.0.1:8080/nonexistent` (renders custom styled body from `error_pages/404.html`)
- 301 Redirect: `http://127.0.0.1:8080/old-page` (redirects automatically to `/index.html`)
- Directory Autoindex: `http://127.0.0.1:8080/listing/` (renders HTML directory table)
- Session Demo: `http://127.0.0.1:8080/session` (increments visit counter on reload)
- Admin Dashboard: `http://127.0.0.1:8080/admin` (auto-refreshing telemetry view)
- JSON Telemetry: `http://127.0.0.1:8080/admin/metrics.json` (raw JSON metrics)

---

## 3. Master 40-Question Reference Table

| # | Question | Key File & Line Reference | Reproducer / Verification | Implementation Summary |
|---|---|---|---|---|
| **A-1** | HTTP server operation | `src/main.rs:1-35`<br>`src/server.rs:252-280` | `curl -i http://127.0.0.1:8080/` | Single-threaded reactor pattern, non-blocking epoll loop |
| **A-2** | I/O multiplexing function | `src/server.rs:171`<br>`src/server.rs:256` | Code inspection | Linux `epoll` via `mio::Poll::poll` (`epoll_wait`) |
| **A-3** | Only ONE epoll instance | `src/server.rs:128`<br>`src/server.rs:171` | Code inspection | Exactly one `Poll` struct owned by `Server` |
| **A-4** | Why & how one epoll | `src/server.rs:63-71`<br>`src/server.rs:265-272` | Code inspection | Avoids multi-loop deadlocks; partitioned token space |
| **A-5** | One read/write per event | `src/server.rs:374-380`<br>`src/connection.rs:307-347` | Code inspection | Mutually exclusive `else if`, single `read()`/`write()` syscall |
| **A-6** | Return values checked | `src/connection.rs:126-155` | `cargo test connection::tests::classify` | `classify_read` & `classify_write` map to `IoOutcome` |
| **A-7** | Client removal on error | `src/server.rs:439-462` | Code inspection | Deregisters token, calls `shutdown`, `Drop` closes fd |
| **A-8** | ALL I/O via epoll | `src/cgi.rs:270-291`<br>`src/server.rs:586-636` | Code inspection | CGI stdio pipes set `O_NONBLOCK`, registered via `SourceFd` |
| **A-9** | Single server, single port | `config.yaml:14-22`<br>`src/server.rs:175-204` | `curl -i http://127.0.0.1:8081/` | Port 8081 listener binds and serves default website |
| **A-10** | Multiple servers & ports | `config.yaml:14,98,123`<br>`src/server.rs:175-204` | `curl 8080`, `curl 8081`, `curl 8082` | Server binds and multiplexes 8080, 8081, 8082 in one loop |
| **A-11** | Virtual hosts on same port | `src/router.rs:72-87`<br>`config.yaml:98-116` | `curl -H "Host: localhost" 8080`<br>`curl -H "Host: test.com" 8080` | Virtual host matching via `Host` header on shared port 8080 |
| **A-12** | Custom error pages | `src/error.rs:27-100`<br>`error_pages/404.html` | `curl -i http://127.0.0.1:8080/missing` | 256KB cap on custom files with compiled-in fallback |
| **A-13** | Client body limit | `src/http/request.rs:305-309`<br>`src/http/chunked.rs:127` | `curl -d "$(head -c 2000 /dev/zero)" 8082/` | Immediate 413 on Content-Length & streaming chunked |
| **A-14** | Routes taken into account | `src/router.rs:93-112` | `tests/audit.sh` (Check A-14) | Longest-prefix matching at segment boundaries |
| **A-15** | Default index file | `src/router.rs:221-236` | `curl http://127.0.0.1:8080/` | Serves `index.html`; redirects missing trailing slash (301) |
| **A-16** | Accepted methods per route | `src/router.rs:169-177` | `curl -X DELETE 8080/readonly/file`<br>`curl -X DELETE 8080/uploads/file` | 405 Method Not Allowed + `Allow:` header; 204 on allowed |
| **A-17** | GET requests work | `src/handlers/mod.rs:83-120` | `curl -i http://127.0.0.1:8080/` | Streams files >64KB in chunks, RFC 9110 headers |
| **A-18** | POST requests work | `src/handlers/mod.rs:145-245`<br>`src/cgi.rs:330-358` | `curl -F "file=@logo.png" 8080/uploads/` | Multipart file upload and CGI POST stream handling |
| **A-19** | DELETE requests work | `src/handlers/mod.rs:125-144` | `curl -X DELETE 8080/uploads/target` | 204 No Content on delete, 404 missing, 403 directory |
| **A-20** | Wrong request resilience | `src/http/request.rs:444-528`<br>`src/server.rs:367-392` | `printf 'GARBAGE\r\n\r\n' \| nc 127.0.0.1 8080` | Returns 400 Bad Request, catch_unwind protects loop |
| **A-21** | Upload & download integrity | `src/http/multipart.rs:73-161`<br>`tests/integration.rs:496` | Upload binary & compare `sha256sum` | Byte-level boundary matching, zero corruption |
| **A-22** | Sessions & cookies | `src/utils/cookie.rs:53-71`<br>`src/utils/session.rs:183` | `curl -c c.txt -b c.txt 8080/session` | `/dev/urandom` 256-bit hex id, HttpOnly, SameSite=Lax |
| **A-23** | Browser connects cleanly | `src/connection.rs:360-435` | Open Chrome/Firefox DevTools | Full header compliance (`Accept`, `Sec-Fetch-*`) |
| **A-24** | Headers correct | `src/http/response.rs:142-231` | `curl -i http://127.0.0.1:8080/` | IMF-fixdate `Date`, `Server`, `Content-Type/Length` |
| **A-25** | Wrong URL handled | `src/router.rs:158`<br>`src/error.rs:27-51` | `curl -i http://127.0.0.1:8080/bad/url` | Resolves to 404 Not Found with custom error page |
| **A-26** | Directory autoindex | `src/handlers/mod.rs:247-330` | `curl http://127.0.0.1:8080/listing/` | Autoindex HTML with escaped filenames & file sizes |
| **A-27** | Redirected URL | `src/router.rs:161-167`<br>`src/connection.rs:866-877` | `curl -i http://127.0.0.1:8080/old-page` | 301 Moved Permanently with `Location: /index.html` |
| **A-28** | CGI chunked & unchunked | `src/http/chunked.rs:88-193`<br>`src/cgi.rs:497-560` | `curl -H "Transfer-Encoding: chunked" ...` | De-chunks stdin to child, parses stdout headers |
| **A-29** | Multiple ports & websites | `config.yaml:1-140` | `tests/audit.sh` | 3 server blocks across 3 distinct listeners |
| **A-30** | Duplicate port conflict | `src/config/validate.rs:245-324` | `cargo run -- config.dup-port.yaml` | Fatal validation abort with exit code 2 |
| **A-31** | Partial bad config | `src/config/validate.rs:334-352` | `cargo run -- config.partial-bad.yaml` | Two-severity design: bad dropped, good servers serve |
| **A-32** | Siege availability >=99.5% | `tests/stress.py:268-274` | `python3 tests/stress.py` | 100.00% availability over 17,000+ transactions |
| **A-33** | No memory leaks | `tests/stress.py:276-285` | `ls /proc/$PID/fd \| wc -l` | Baseline FDs = 7, Final FDs = 7 (diff = 0) |
| **A-34** | No hanging connections | `src/server.rs:847-893`<br>`tests/stress.py:287` | `ss -tan \| grep 8080` | Dual 30s idle / 60s total timeouts; 0 CLOSE_WAIT |
| **A-35** | `cargo test` all pass | `tests/integration.rs`<br>`src/**/tests.rs` | `cargo test` | 262 passed (246 unit + 16 integration) |
| **A-36** | HTTP parser tests | `src/http/request.rs:560-890` | `cargo test http::request` | 33 request + 12 chunked unit tests |
| **A-37** | Config validation tests | `src/config/validate.rs:366-697` | `cargo test config::validate` | 21 validate + 20 parser unit tests |
| **A-38** | Route matching tests | `src/router.rs:306-440` | `cargo test router::tests` | 25 router unit tests |
| **A-39** | Multiple CGI systems | `config.yaml:79-82`<br>`cgi-bin/{hello.py,info.pl}` | `curl 8080/cgi-bin/hello.py`<br>`curl 8080/cgi-bin/info.pl` | Extension-based dispatch: Python 3 and Perl |
| **A-40** | Admin metrics dashboard | `src/metrics.rs:1-199`<br>`src/handlers/admin_page.rs` | `curl 8080/admin`<br>`curl 8080/admin/metrics.json` | Lock-free telemetry + auto-refreshing dashboard |

---

## 4. Detailed Categorized Walkthrough

### 4.1 Functional Architecture & I/O Multiplexing (A-1..A-8)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-1** (How HTTP works) | `src/main.rs:1-35`<br>`src/server.rs:252-280` | Show the `Server::run` loop. Explain the request-response cycle, parsing headers, routing, executing handlers, and framing RFC 9110 responses. |
| **A-2** (I/O multiplexing) | `src/server.rs:171, 256` | Show `Poll::new()?` wrapping `epoll_create1` and `self.poll.poll(...)` calling `epoll_wait(2)`. Explain O(ready) performance without linear scans. |
| **A-3** (Only ONE epoll) | `src/server.rs:128, 171` | Show `struct Server { poll: Poll, ... }`. Show that there are no worker threads, thread pools, or additional `Poll` instances anywhere. |
| **A-4** (Why & how one epoll) | `src/server.rs:63-71, 265-272` | Explain token space partitioning: `0..1024` for listeners, `1024..` for client sockets, and high-range for CGI stdio pipes. Single loop prevents deadlocks. |
| **A-5** (One read/write per tick) | `src/server.rs:374-380`<br>`src/connection.rs:307-347` | Show `if event.is_readable() { ... } else if event.is_writable() { ... }`. Show `read_once` and `write_once` executing exactly one syscall without a loop. |
| **A-6** (I/O return values) | `src/connection.rs:126-155` | Show `classify_read` and `classify_write`. Explain that `WouldBlock` preserves connection state, while `Failed(err)` triggers teardown. |
| **A-7** (Client removal on error) | `src/server.rs:439-462` | Show `Server::close(token)`. It deregisters from `Poll`, calls `shutdown(Both)`, and removes the client from `self.connections`, triggering RAII `Drop` to close the fd. |
| **A-8** (ALL I/O via epoll) | `src/cgi.rs:270-291`<br>`src/server.rs:586-636` | Show CGI stdin/stdout set to `O_NONBLOCK`, wrapped in `mio::unix::SourceFd`, and registered directly in `self.poll.registry()`. |

### 4.2 Configuration & Virtual Hosts (A-9..A-16, A-29)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-9** (Single server/port) | `config.yaml:14-22` | Show port 8081 config. Run `curl -i http://127.0.0.1:8081/` -> 200 OK. |
| **A-10** (Multiple ports) | `config.yaml:14, 98, 123` | Show 8080, 8081, 8082 in config. All three listeners registered in `server.rs:175-204`. |
| **A-11** (Virtual hosts) | `src/router.rs:72-87` | Show `select_server`: matches incoming `Host:` against `server_names`, falls back to `is_default`. |
| **A-12** (Custom error pages) | `src/error.rs:27-100` | Show `custom_error_page`: checks configured map, reads with 256KB ceiling, falls back to infallible compiled-in HTML. |
| **A-13** (Client body limit) | `src/http/request.rs:305`<br>`src/http/chunked.rs:127` | Show 413 checks on `Content-Length` header and during incremental chunked decoding. |
| **A-14** (Route resolution) | `src/router.rs:93-112` | Show `match_route`: longest prefix match verified at segment boundaries (`/uploads` matches `/uploads/foo` but not `/uploadsfake`). |
| **A-15** (Default index file) | `src/router.rs:221-236` | Show index resolution: if target is a directory, appends configured `index: index.html`. If missing trailing slash, issues 301. |
| **A-16** (Accepted methods) | `src/router.rs:169-177` | Show method validation against `route.accepted_methods`. Returns 405 Method Not Allowed with valid `Allow:` header. |

### 4.3 Methods, Uploads, Cookies & Sessions (A-17..A-22)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-17** (GET requests) | `src/handlers/mod.rs:83-120` | Show `serve_file`: files >64KB streamed in chunks without full buffering. RFC 9110 compliant headers. |
| **A-18** (POST requests) | `src/handlers/mod.rs:145-245` | Show `handle_upload`: parses multipart form bodies or raw streams, writes files to `upload_dir`. |
| **A-19** (DELETE requests) | `src/handlers/mod.rs:125-144` | Show `handle_delete`: deletes file and returns 204 No Content. Returns 404 if missing, 403 if directory. |
| **A-20** (Wrong requests) | `src/http/request.rs:444`<br>`src/server.rs:367` | Show 400 Bad Request generation on garbage syntax. Show `catch_unwind` in `server.rs` isolating per-client panics. |
| **A-21** (Upload integrity) | `src/http/multipart.rs:73-161` | Show byte-exact boundary searching. Demonstrated by `test_f_file_upload_and_roundtrip` (SHA256 binary match). |
| **A-22** (Cookies & Sessions) | `src/utils/session.rs:183`<br>`src/utils/cookie.rs:53` | Show session generation from `/dev/urandom` (256-bit hex). Show `SetCookie` builder applying `HttpOnly` and `SameSite=Lax`. |

### 4.4 Browser Interaction & CGI (A-23..A-28)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-23** (Browser compatibility) | `src/connection.rs:360-435` | Handles browser-specific headers (`Sec-Fetch-*`, `User-Agent`, conditional headers). |
| **A-24** (Headers correctness) | `src/http/response.rs:142-231` | Show IMF-fixdate generator, `Server: localserver/0.1.0`, `Content-Length`, `Connection: keep-alive`. |
| **A-25** (Wrong URL) | `src/router.rs:158` | Path sanitization detects non-existent target or traversal, returns clean 404 Not Found. |
| **A-26** (Autoindex) | `src/handlers/mod.rs:247-330` | Generates HTML directory table with escaped entry names, byte sizes, and directory links. |
| **A-27** (Redirects) | `src/router.rs:161-167`<br>`src/connection.rs:866` | Route redirect returns 301 Moved Permanently with `Location: <dest>` and HTML-escaped body. |
| **A-28** (CGI unchunked/chunked) | `src/http/chunked.rs:88`<br>`src/cgi.rs:497-560` | De-chunks chunked client bodies before pumping to CGI child stdin; parses CGI stdout headers. |

### 4.5 Configuration Conflicts & Resilience (A-29..A-31)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-30** (Duplicate port conflict) | `src/config/validate.rs:245-324` | Detects colliding `(host:port, server_name)` or multiple default servers on same port. Aborts startup with exit code 2. |
| **A-31** (Partial bad configuration) | `src/config/validate.rs:334-352` | Two-severity design: fatal listener conflicts abort; bad server root/error-page is skipped with warning, allowing good servers to run. |

### 4.6 Stress Testing & Leak Verification (A-32..A-34)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-32** (Siege availability >=99.5%) | `tests/stress.py:268-274` | Run `python3 tests/stress.py`: 100.00% availability over 17,000+ transactions at 1,732 req/s. |
| **A-33** (No memory / FD leaks) | `tests/stress.py:276-285` | Samples `/proc/$PID/fd` before and after test: baseline = 7, final = 7 (diff = 0). Proves zero FD leaks. |
| **A-34** (No hanging connections) | `src/server.rs:847-893`<br>`tests/stress.py:287` | Dual timeouts: 30s idle and 60s hard deadline. `ss -tan | grep 8080` shows 0 `CLOSE_WAIT` connections. |

### 4.7 Test Suites & Bonus Features (A-35..A-40)

| Question | Source Pointer | What to Point at & Explain |
|---|---|---|
| **A-35** (`cargo test` all pass) | `tests/integration.rs` | 262 tests passing (246 unit + 16 live TCP integration tests). |
| **A-36** (HTTP parser tests) | `src/http/request.rs:560-890` | 33 unit tests for method parsing, headers, chunked framing, limits, malformed syntax. |
| **A-37** (Config validation tests) | `src/config/validate.rs:366-697` | 21 validate + 20 parser unit tests covering syntax errors, duplicate ports, and body sizes. |
| **A-38** (Route matching tests) | `src/router.rs:306-440` | 25 unit tests covering longest prefix, method permissions, index, autoindex, traversal. |
| **A-39** (Multiple CGI systems) | `config.yaml:79-82`<br>`cgi-bin/{hello.py,info.pl}` | Extension-to-interpreter map: `.py` -> Python 3, `.pl` -> Perl. Tested live in integration suite. |
| **A-40** (Admin metrics dashboard) | `src/metrics.rs:1-199`<br>`src/handlers/admin_page.rs` | Lock-free metrics collector and HTML dashboard at `/admin`, JSON API at `/admin/metrics.json`. |
