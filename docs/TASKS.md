# LocalServer — Implementation Tasklist

Step-by-step plan to build the HTTP/1.1 server from `exercise.md`, ordered so that
every task is testable on its own. Each task states **what**, **why** (the reasoning
you must be able to defend at the audit), and **which audit questions it answers**.

Audit questions are referenced as `[A-n]`; the full mapping is at the bottom.

**Environment note:** `rustc`/`cargo` 1.97.1 present. `siege` is **not installed** —
install before Phase 9 (`sudo apt install siege` or build from
https://github.com/JoeDog/siege).

## Tracker — status at a glance (see `REPORT.md` for the full report)

> **Legend:** ✅ done & tested · 🟨 code correct / automation missing · ⬜ not started. Updated 2026-09-10; `cargo test` 221 passed, `clippy` 0 warnings. Progress: **32/40 done, 3 partial, 5 todo** (mandatory remainder: 9.1 integ +9.2+9.3+10.1+10.2; plus bonus 8.1).

| ID | Phase — Task | Status | Audit | Source |
|---|---|---|---|---|
| 0.1 | Cargo + module tree | ✅ DONE | — | `Cargo.toml:12,24` `src/**/*.rs` |
| 0.2 | Repo scaffolding | ✅ DONE | — | `config.yaml` `www/` `error_pages/` `cgi-bin/` `uploads/` |
| 1.1 | Config data model | ✅ DONE | A-9..16 | `src/config/mod.rs:40` |
| 1.2 | Hand-written parser | ✅ DONE | A-30,37 | `src/config/parser.rs` |
| 1.3 | Startup validation | ✅ DONE | A-30,31 | `src/config/validate.rs:41` |
| 1.4 | Tests: config | ✅ DONE | A-37 | `validate.rs:366` 41 tests |
| 2.1 | Listeners in one `Poll` | ✅ DONE | A-11 | `src/server.rs:159` |
| 2.2 | The loop | ✅ DONE | A-2..4,8 | `src/server.rs:252` |
| 2.3 | Accept loop | ✅ DONE | A-20 | `src/server.rs:288` |
| 2.4 | One read/write per event | ✅ DONE | A-5 | `src/connection.rs:303` |
| 2.5 | Error & client removal | ✅ DONE | A-6,7,33 | `connection.rs:126` `server.rs:439` |
| 2.6 | Timeouts | ✅ DONE | A-34 | `connection.rs:614` `server.rs:819` |
| 2.7 | Never crash | ✅ DONE | A-20 | `server.rs:165,367` `connection.rs:63` |
| 3.1 | Request line + headers | ✅ DONE | A-36 | `src/http/request.rs` |
| 3.2 | Body: CL + chunked | ✅ DONE | A-13,28 | `request.rs:299` `chunked.rs` |
| 3.3 | Multipart upload | ✅ DONE | A-21 | `src/http/multipart.rs` |
| 3.4 | Response builder | ✅ DONE | A-24 | `src/http/response.rs` |
| 3.5 | Tests: HTTP parsing | ✅ DONE | A-36 | `request.rs:560` |
| 4.1 | Vhost selection | ✅ DONE | A-11,29 | `src/router.rs:72` |
| 4.2 | Route matching | ✅ DONE | A-14..16,27 | `router.rs:93` |
| 4.3 | Path sanitising | ✅ DONE | A-25 | `src/utils/path.rs` |
| 4.4 | Tests: route matching | ✅ DONE | A-38 | `router.rs:306` |
| 5.1 | GET | ✅ DONE | A-17,33 | `src/handlers/mod.rs:83` |
| 5.2 | POST | ✅ DONE | A-18,21 | `handlers/mod.rs:145` |
| 5.3 | DELETE | ✅ DONE | A-19,16 | `handlers/mod.rs:130` |
| 5.4 | Directory listing | ✅ DONE | A-26 | `handlers/mod.rs:247` |
| 5.5 | Error pages | ✅ DONE | A-12 | `src/error.rs:31` |
| 6.1 | Cookie utils | ✅ DONE | A-22 | `src/utils/cookie.rs` |
| 6.2 | Session store + `/session` | ✅ DONE | A-22 | `session.rs` `handlers/session_page.rs` |
| 7.1 | Non-blocking CGI | ✅ DONE | A-8 | `src/cgi.rs:210` `server.rs:514` |
| 7.2 | CGI environment | ✅ DONE | — | `cgi.rs:89` |
| 7.3 | CGI translation + timeout | ✅ DONE | A-28 | `cgi.rs:497` `server.rs:766` |
| 7.4 | Second CGI (`info.pl`) | ✅ DONE | **A-39** bonus | `cgi-bin/info.pl` `config.yaml:79` |
| 8.1 | `/admin` metrics | ⬜ TODO (bonus) | **A-40** bonus | — |
| 9.1 | `cargo test` suite | 🟨 PARTIAL | A-35 | unit ✅ `tests/*.rs` ❌ |
| 9.2 | `tests/audit.sh` | ⬜ TODO | A-9..22 | — |
| 9.3 | `siege` stress `≥99.5%` | ⬜ TODO | A-32..34 | binary missing |
| 9.4 | Leak/hanging checks | 🟨 PARTIAL | A-33,34 | code ✅ automation ❌ |
| 9.5 | Browser pass | 🟨 PARTIAL | A-23..28 | manual ✅ checklist ❌ |
| 9.6 | `README.md` | ✅ DONE | A-1..8 | `README.md` |
| 10.1 | `AUDIT_NOTES.md` | ⬜ TODO | A-1..8 | — |
| 10.2 | `config.dup-port.yaml` / `config.partial-bad.yaml` | ⬜ TODO | A-30,31 | — |

---

## Architecture decisions (settle these before writing code)

These are the answers the audit is actually probing for. Decide them once, here,
and let every task below follow from them.

| Decision | Choice | Rationale |
|---|---|---|
| Concurrency model | **One process, one thread, one `mio::Poll`** | Required by the subject. Everything — listeners, client sockets, CGI pipes — is a `Token` in the same event loop. No `std::thread`, no channels between threads. |
| I/O crate | `mio` (thin `epoll` wrapper) + `libc` for `fcntl`/`waitpid` | Subject forbids `tokio`/`async-std`/`hyper`. `mio` gives readiness events only — no runtime, no executor, no futures. |
| Readiness style | **Edge-ish, but one syscall per readiness event** | Never loop `read()` until `WouldBlock` inside one event. One `read` or one `write` per client per poll wake-up (`[A-5]`). Level-triggered registration means the next `poll()` re-reports remaining data. |
| Buffering | Per-connection `read_buf` / `write_buf` `Vec<u8>` owned by the connection struct | A partial request or a partially-flushed response must survive across event-loop iterations. No blocking calls to "finish" anything. |
| Config format | **Hand-written parser** for an indentation-based YAML subset | Keeps dependencies to `mio` + `libc`, and makes the Configuration Validation unit tests meaningful (you wrote the validation, so you can explain it). |
| Error policy | No `unwrap`/`expect`/`panic!` on any request path | "Never crash." Every `io::Result` is matched; fatal socket errors drop the client (`[A-7]`). |
| CGI | Fork via `std::process::Command` with piped stdio; **pipes registered in the same Poll** | A blocking `child.wait_with_output()` would stall every other client — the single most common audit failure. |

---

## Phase 0 — Project skeleton

### Task 0.1 — Cargo project + module tree
**Do:** `cargo init --name localserver`. Create the module tree:
```
src/main.rs                  entry point: parse args, load config, start server
src/config/mod.rs            config structs (ServerConfig, RouteConfig)
src/config/parser.rs         hand-written YAML-subset parser
src/config/validate.rs       startup validation (ports, paths, body limits)
src/server.rs                event loop, Poll, listeners, connection registry
src/connection.rs            per-client state machine (ReadingHeaders/ReadingBody/Writing/Closing)
src/http/mod.rs              http module re-exports
src/http/request.rs          request parser (request line, headers, body)
src/http/chunked.rs          chunked transfer decoder/encoder
src/http/multipart.rs        multipart/form-data parser + filename sanitising
src/http/response.rs         response builder + serialization
src/router.rs                host+path -> route resolution
src/handlers/mod.rs          static files, upload, delete, directory listing, redirect
src/handlers/session_page.rs /session demo page — visit counter, sign-in/out
src/cgi.rs                   CGI process spawn + non-blocking pipe pumping
src/error.rs                 status codes -> error pages
src/utils/mod.rs             utils module re-exports
src/utils/session.rs         in-memory session store
src/utils/cookie.rs          Cookie / Set-Cookie parse & build
src/utils/mime.rs            extension -> Content-Type
src/utils/path.rs            path resolution + traversal sanitising
```
Add `mio = { version = "1", features = ["os-poll", "net", "os-ext"] }` and `libc` (`os-ext` provides `SourceFd` for CGI pipes — see Task 7.1).

> **Note:** `src/http/multipart.rs` (Phase 3.3) and `src/handlers/session_page.rs` (Phase 6.2) were introduced after the initial skeleton but are part of the canonical tree above; the initial printed tree omitted them.

**Why:** The audit asks you to *show the code*. A flat `main.rs` makes every
question ("show me where you check the return of `write`") an archaeology exercise.
Modularity is also an explicit tip in the subject.

**Done when:** `cargo build` succeeds with empty modules; `cargo test` runs 0 tests.

### Task 0.2 — Repo scaffolding
**Do:** Create `config.yaml` (real, used by default), `www/` with a small static
site (`index.html`, CSS, an image, a subdirectory with no index for listing tests),
`error_pages/` (400, 403, 404, 405, 413, 500 HTML files), `cgi-bin/` (empty for now),
`uploads/` (writable), and `README.md` skeleton.

**Why:** "Avoid hardcoding; use the config file." The auditor will *edit* `config.yaml`
and re-run (`[A-9..16]`), so real directories must exist for the config to point at.

---

## Phase 1 — Configuration

### Task 1.1 — Config data model
**Do:** Define:
```rust
struct Config { servers: Vec<ServerConfig> }
struct ServerConfig {
    host: String,            // e.g. 127.0.0.1
    ports: Vec<u16>,         // one server may bind several ports
    server_names: Vec<String>,   // virtual host names
    is_default: bool,        // default server for its host:port
    client_max_body_size: usize,
    error_pages: HashMap<u16, PathBuf>,
    routes: Vec<RouteConfig>,
}
struct RouteConfig {
    path: String,               // URL prefix, e.g. /uploads
    methods: Vec<Method>,       // accepted methods
    root: PathBuf,              // filesystem root for this prefix
    index: Option<String>,      // default file when path is a directory
    autoindex: bool,            // directory listing toggle
    redirect: Option<(u16, String)>,  // (301|302, target)
    cgi: HashMap<String, PathBuf>,    // ".py" -> /usr/bin/python3
    upload_dir: Option<PathBuf>,
}
```
**Why:** This struct list *is* the "Configuration File" section of the subject —
every bullet there maps to a field. If a field is missing, that config feature
cannot work, and the auditor tests each one individually (`[A-9..16]`).

### Task 1.2 — Hand-written parser
**Do:** Parse an indentation-based YAML subset: nested maps, `- ` lists, scalars,
`#` comments, quoted strings, sizes like `1M`/`10K`. Return
`Result<Config, ConfigError>` with **line numbers** in the error.

**Why:** A parse error must be a clean startup failure with a useful message, not a
panic. Line numbers are what convinces an auditor the validation is real.

### Task 1.3 — Startup validation
**Do:** In `validate.rs`, reject or report:
1. **Duplicate listeners** — same `host:port` **and** an overlapping `server_name`
   (or two `is_default: true` on the same `host:port`) → **fatal error, exit non-zero**.
   Same `host:port` with *distinct* server names is legal virtual hosting, not an error.
2. Route `root` does not exist / is not a directory.
3. Error-page file missing or unreadable.
4. `client_max_body_size` == 0 or above a sane cap.
5. Route `path` not starting with `/`; empty method list; unknown method.
6. CGI interpreter path not executable.
7. Redirect status not in {301, 302, 303, 307, 308}.

Two severities: **fatal** (kills startup, e.g. duplicate port) vs **per-server
skip** (log the reason, drop that one `ServerConfig`, keep serving the rest;
abort only if *zero* servers survive).

**Why:** Directly `[A-30]` ("configure the same port twice — the server should find
the error") and `[A-31]` ("one broken config must not take the whole server down").
The two-severity design is the answer to "why should the server still work if one
config is invalid?": a misconfigured *virtual host* is isolated, but a genuine
*port conflict* is unresolvable and must fail loudly at startup rather than at
`bind()` time in production.

### Task 1.4 — Unit tests: configuration validation
**Do:** Tests for: duplicate host:port+name is rejected; same port + different
server_names is accepted; nonexistent root rejected; body size 0 rejected;
malformed YAML gives an error with the right line; one invalid server among three
leaves two running.

**Why:** Explicitly required — `[A-37]`, and the "Configuration Validation" bullet
of the subject's Unit Tests section.

---

## Phase 2 — The event loop

### Task 2.1 — Listeners in one Poll
**Do:** For each unique `host:port` across all servers, create one
`mio::net::TcpListener`, register it with `Interest::READABLE` under a token from a
listener-token range. Keep `HashMap<Token, Vec<ServerConfig>>` — the list of virtual
hosts sharing that socket.

**Why:** One socket per `host:port`, *not* per server block, is what makes virtual
hosting (`[A-11]`) work: two servers on `127.0.0.1:8080` share a listener and are
told apart later by the `Host` header. It is also why duplicate-port detection has
to happen in config validation, not at bind time.

### Task 2.2 — The loop itself
**Do:**
```rust
loop {
    poll.poll(&mut events, Some(Duration::from_millis(500)))?;  // timeout => timer tick
    for event in events.iter() {
        match token_kind(event.token()) {
            Listener   => accept_new_connections(...),
            Connection => handle_connection_event(...),   // readable and/or writable
            CgiPipe    => handle_cgi_event(...),
        }
    }
    sweep_timeouts(...);       // idle + total-request deadlines
    reap_finished_cgi(...);    // waitpid(WNOHANG)
    sessions.sweep_expired();
}
```
A single `Poll`, a single `Events` buffer, one `HashMap<Token, Connection>`,
monotonically increasing token counter with free-list reuse.

**Why:** This is the core of `[A-2]`, `[A-3]`, `[A-4]`, `[A-8]`. Be ready to say:
`epoll` (via `mio`) is used because it scales O(ready) rather than O(watched) as
`select` does and has no `FD_SETSIZE` limit. **One** epoll instance means a single
authority on readiness — with two, a socket could be readable in one while the
thread blocks in the other's `wait()`, and you'd have to poll-spin or thread. The
`poll()` timeout is what guarantees the loop still wakes to enforce timeouts when
no traffic arrives.

### Task 2.3 — Accept loop
**Do:** On a listener event, `accept()` in a loop until `WouldBlock`; for each
socket set `TCP_NODELAY`, register `Interest::READABLE`, store a `Connection` with
`created_at`/`last_activity`. On `WouldBlock` stop; on any other error log and
**continue serving** (never exit the loop).

**Why:** Accepting until `WouldBlock` is the one place a drain loop is correct —
each `accept()` returns a *different* fd, so it is not "multiple reads on one
client". A hard-failing `accept` (e.g. EMFILE) must never kill the server: that is
the "never crash" requirement.

### Task 2.4 — One read / one write per event
**Do:** In `handle_connection_event`: if readable → **exactly one** `read()` into
the connection buffer, then advance the state machine on the bytes you have. If
writable → **exactly one** `write()` of the pending slice, advance the write cursor.
Re-register interest based on state (READABLE while parsing, WRITABLE while a
response is pending, both never needed simultaneously for the same phase).

**Why:** `[A-5]` is a literal question: *is there only one read or write per client
per epoll?* Looping until `WouldBlock` on a single client lets one slow-loris or
fast-uploading client monopolise the thread. One syscall per event = fair
round-robin across clients, and level-triggered epoll re-reports the remainder on
the next iteration, so nothing is lost.

### Task 2.5 — Error handling and client removal
**Do:** Central helper: match on `io::Result`. `Ok(0)` → peer closed → deregister
and drop. `Ok(n)` → progress, refresh `last_activity`. `Err(WouldBlock)`/`Interrupted`
→ **not** an error, return and wait for the next event. Any other `Err` → deregister,
`shutdown()`, drop the `Connection` (which closes the fd via `Drop`), and drop any
CGI child it owned.

**Why:** `[A-6]` and `[A-7]`. Note explicitly that `WouldBlock` must not remove the
client — treating it as an error is the classic bug. And that removing the
connection from the `HashMap` is what closes the fd, which is the file-descriptor
leak defence (`[A-33]`).

### Task 2.6 — Timeouts
**Do:** Two deadlines per connection: idle (no bytes moved for N seconds, default 30)
and total request duration (default 60). Idle-timeout while reading headers/body →
`408`; while writing → just close. Swept on every loop iteration.

**Why:** "Timeout long requests" in the subject; `[A-34]` ("no hanging connection").
Without it a `telnet` session that sends `GET / HTTP/1.1\r\n` and stops holds an fd
forever, and `siege` runs will leave a growing pile of half-open sockets.

### Task 2.7 — Never crash
**Do:** `#![deny(clippy::unwrap_used, clippy::expect_used)]` on request-path modules
(allow in tests). Ignore `SIGPIPE` (`libc::signal(SIGPIPE, SIG_IGN)`) at startup.
Wrap per-connection handling in `std::panic::catch_unwind` as a last-resort net that
turns a logic bug into a `500` + dropped client instead of a dead process.

**Why:** "Never crash" is a hard requirement and the first thing the auditor
stress-tests. Ignoring `SIGPIPE` matters specifically because a browser closing a
tab mid-response would otherwise kill the process with a signal.

---

## Phase 3 — HTTP parsing

### Task 3.1 — Request line + headers
**Do:** Incremental parser over the accumulated buffer: find `\r\n\r\n`; parse
`METHOD SP TARGET SP HTTP/1.1`; parse headers into a case-insensitive multimap.
Reject → `400`: bad version, non-token method, missing `Host` (HTTP/1.1), header
block over ~8 KB, invalid chars, duplicate/conflicting `Content-Length`. Return
`Incomplete` when the terminator is not there yet rather than erroring.

**Why:** The parser must be *restartable* — data arrives in arbitrary chunks
because of one-read-per-event. `Incomplete` vs `Error` is the distinction that keeps
`[A-5]` and correct parsing compatible.

### Task 3.2 — Body: Content-Length and chunked
**Do:** After headers: if `Transfer-Encoding: chunked` → feed bytes to the chunked
decoder state machine (`ReadSize → ReadData → ReadCrlf → Trailers → Done`); else if
`Content-Length: n` → read exactly n bytes; else no body. Enforce
`client_max_body_size` **during** accumulation → `413` as soon as it is exceeded,
without reading the rest.

**Why:** Subject requires chunked *and* unchunked (`[A-28]`). Early `413` matters:
`[A-13]` tests a body over the limit, and buffering a 2 GB upload before rejecting
it is a trivial memory-exhaustion vector.

### Task 3.3 — Multipart upload parsing
**Do:** For `Content-Type: multipart/form-data; boundary=...`, split parts, read each
part's headers, extract `filename`, sanitise it (strip path separators, `..`, control
chars, reject empty/hidden), write the exact bytes to the route's `upload_dir`.
Support plain `POST` bodies written to a file too.

**Why:** `[A-21]` — upload a file and download it back *byte-identical*. Test with a
binary (PNG/tarball) and `sha256sum` both sides; a text-mode or boundary off-by-one
bug shows up only on binary data.

### Task 3.4 — Response builder
**Do:** `Response { status, headers, body }` → bytes. Always set `Date`, `Server`,
`Content-Length` (or `Transfer-Encoding: chunked`), `Content-Type` from
`utils/mime.rs`. Honour `Connection: keep-alive` (default in 1.1) and `close`.
Correct status text for every code used.

**Why:** `[A-24]` in the browser section — "are the request and response headers
correct?". Browsers hang on a missing/incorrect `Content-Length`, which reads at the
audit as "the server is broken".

### Task 3.5 — Unit tests: HTTP request parsing
**Do:** Test: GET with headers; POST with `Content-Length`; DELETE; chunked body
reassembly (including a body split across three feed calls); header-only prefix
returns `Incomplete`; missing `Host` → 400; bad version → 400; oversized body → 413;
case-insensitive header lookup; multipart filename extraction and sanitising.

**Why:** Required by the subject and `[A-36]`. The split-feed test is the one that
proves the parser really is incremental.

---

## Phase 4 — Routing

### Task 4.1 — Virtual host selection
**Do:** From the accept listener, get the candidate `ServerConfig`s. Match the
`Host` header (strip `:port`, lowercase) against `server_names`; no match → the
server flagged `is_default` for that `host:port`, else the first.

**Why:** `[A-11]` — `curl --resolve test.com:8080:127.0.0.1 http://test.com/` must
reach a different site than `curl http://127.0.0.1:8080/`. Demonstrate both.

### Task 4.2 — Route matching
**Do:** Longest-prefix match of the URL path against `RouteConfig.path`. Then, in
order: (1) redirect → return 3xx immediately; (2) method not in `methods` → `405`
with an `Allow` header; (3) map the remainder onto `root`; (4) extension in `cgi` →
CGI; (5) path is a directory → `index` file if present, else `autoindex` listing,
else `403`; (6) otherwise serve the file.

**Why:** `[A-14]`, `[A-15]`, `[A-16]`, `[A-27]`. Order matters and you will be asked
about it: a redirect must win over method checking (you're not serving the resource),
and `405` must precede `404` (the resource's existence is not the question).

### Task 4.3 — Path sanitising
**Do:** Percent-decode, reject `%00`, normalise `.`/`..` segments *before* touching
the filesystem, then verify the canonicalised result is still under `root` → else
`403`. Never pass user input to `Path::join` unnormalised.

**Why:** `GET /../../etc/passwd` is the single test an auditor is most likely to try
unprompted. Normalising before `canonicalize` also handles the non-existent-path case
where `canonicalize` fails outright.

### Task 4.4 — Unit tests: route matching + status codes
**Do:** Longest-prefix wins over shorter; method rejection yields 405 + `Allow`;
extension maps to the right CGI interpreter; redirect route returns configured
status/target; directory + `index` resolves to the file; directory without index and
`autoindex:false` → 403; traversal attempt → 403; missing file → 404.

**Why:** `[A-38]` (Route Matching tests) and the subject's "Status Code Generation"
bullet.

---

## Phase 5 — Methods, static files, errors

### Task 5.1 — GET
**Do:** Serve regular files: open, `Content-Length` from metadata, stream through the
write buffer in bounded chunks (e.g. 64 KB per writable event) rather than loading
whole files. `HEAD` = GET without the body.

**Why:** `[A-17]`. Chunked streaming keeps one big download from blocking the loop
and keeps memory flat under `siege` (`[A-33]`).

### Task 5.2 — POST
**Do:** Route-configured behaviour: upload to `upload_dir`, or hand the body to CGI.
Return `201` with a `Location` for a created upload, `200` otherwise.

**Why:** `[A-18]` and `[A-21]`.

### Task 5.3 — DELETE
**Do:** Resolve within the route root, delete the file, `204` on success, `404` if
absent, `403` if not permitted/not writable — and `405` if DELETE isn't in the
route's `methods`.

**Why:** `[A-19]` and `[A-16]` explicitly test DELETE *with and without* permission.
Configure one route allowing DELETE and one not, and show both.

### Task 5.4 — Directory listing
**Do:** When `autoindex: true` and no index file, generate an HTML listing: name,
size, mtime, links that work for nested dirs (trailing-slash redirect when missing),
`..` parent link, HTML-escaped names.

**Why:** `[A-26]` "try to list a directory". The trailing-slash redirect is the
detail that makes nested browsing work in a real browser instead of producing broken
relative links.

### Task 5.5 — Error pages
**Do:** `error.rs` maps a status to config `error_pages[code]`; if the file is
missing or unreadable, fall back to a built-in HTML string compiled into the binary.
Cover 400, 403, 404, 405, 408, 413, 500, 501, 505.

**Why:** `[A-12]` (custom error pages) plus the subject's mandatory list. The
built-in fallback is important: an error page that itself 500s is an infinite loop,
and this is exactly the "never crash" edge the auditor pokes at.

---

## Phase 6 — Cookies and sessions

### Task 6.1 — Cookie utilities
**Do:** Parse the `Cookie:` header into pairs; build `Set-Cookie` with `Path`,
`HttpOnly`, `Max-Age`, `SameSite=Lax`.

### Task 6.2 — Session store
**Do:** In-memory `HashMap<SessionId, Session { data, expires_at }>`. IDs from a
non-guessable source (`/dev/urandom` via `std::fs::File`, hex-encoded, 32 bytes).
On request: read the session cookie, look it up, create + `Set-Cookie` if absent or
expired. Sweep expired sessions in the loop tick.

**Do also:** A small demo page (`/session`) showing visit count / login name so the
behaviour is visible in the browser's devtools cookie panel.

**Why:** `[A-22]`. "Show me it works" means the auditor opens devtools → Application →
Cookies. A demo page turns an invisible feature into a visible one. Note: a single
`HashMap` needs no locking because there is only one thread — that is a defensible
answer to the subject's "use thread-safe data structures" tip, and worth saying out loud.

---

## Phase 7 — CGI

### Task 7.1 — Non-blocking CGI execution
**Do:** `Command::new(interpreter).arg(script_path)` with `Stdio::piped()` on all
three streams. Take the child's stdin/stdout fds, set `O_NONBLOCK` (`libc::fcntl`),
wrap in `mio::unix::SourceFd`, **register them in the same Poll** with CGI tokens
linked back to the owning connection. Pump the request body into stdin on WRITABLE
(one write per event), collect stdout on READABLE (one read per event), close stdin
when the body is fully written. Reap with `waitpid(WNOHANG)` in the loop tick.

**Why:** The single most important task in the project after the event loop.
`[A-8]` asks whether *all* reading and writing goes through epoll — CGI pipes are
reads and writes, so `wait_with_output()` (which blocks) fails that question outright
and stalls every other client for the script's whole runtime.

### Task 7.2 — CGI environment
**Do:** Set `REQUEST_METHOD`, `PATH_INFO` (full filesystem path of the script, as the
subject demands), `PATH_TRANSLATED`, `SCRIPT_NAME`, `SCRIPT_FILENAME`, `QUERY_STRING`,
`CONTENT_LENGTH`, `CONTENT_TYPE`, `SERVER_PROTOCOL`, `SERVER_NAME`, `SERVER_PORT`,
`REMOTE_ADDR`, `GATEWAY_INTERFACE=CGI/1.1`, and `HTTP_*` for each request header.
Set the child's **working directory to the script's directory** (`current_dir`).

**Why:** "Ensure correct relative path handling" — a script doing
`open("data.txt")` must resolve relative to itself, not to wherever the server was
launched. `PATH_INFO` is named explicitly in the subject and will be asked about.

### Task 7.3 — CGI response translation
**Do:** Parse the CGI's own header block (`Status:`, `Content-Type:`, `Location:`,
custom headers) off the top of stdout; default to `200 OK` + `text/plain`. If the
script emits no `Content-Length`, buffer or respond `Transfer-Encoding: chunked`.
Enforce a CGI timeout: kill the child (`SIGKILL`), close its pipes, respond `502`/`504`.

**Why:** `[A-28]` — "does CGI work with chunked and unchunked data?" — means both
directions: a chunked *request* body must be de-chunked before reaching stdin, and a
CGI response of unknown length must go back out chunked. The timeout is what keeps an
infinite-loop script from being a permanent fd + process leak.

### Task 7.4 — CGI scripts + a second interpreter (bonus)
**Do:** `cgi-bin/hello.py` (echoes env + body), `cgi-bin/form.py` (reads POST body),
plus a second language — Perl or a compiled C++ binary — registered under a different
extension in the same route's `cgi` map.

**Why:** `[A-39]` bonus: "more than one CGI system". Because the interpreter is a
config value keyed by extension, this bonus should be config-only work if Task 7.1 is
done right — which is itself a good thing to show the auditor.

---

## Phase 8 — Bonus: metrics dashboard

### Task 8.1 — `/admin` metrics endpoint
**Do:** Counters in the event loop: uptime, total/active connections, requests by
method, responses by status class, bytes in/out, CGI spawned/failed/timed-out, open
fd count (`/proc/self/fd`), active sessions. Serve JSON at `/admin/metrics.json` and a
small self-refreshing HTML page at `/admin`.

**Why:** `[A-40]` bonus. It doubles as your own debugging tool: the fd counter is the
fastest way to prove "no fd leak" and "no hanging connections" (`[A-33]`, `[A-34]`)
during a `siege` run, live, in front of the auditor.

---

## Phase 9 — Testing and hardening

### Task 9.1 — Full `cargo test` suite
**Do:** Consolidate Phases 1/3/4 unit tests, add integration tests in `tests/` that
boot the server on an ephemeral port and drive it with `std::net::TcpStream`:
keep-alive reuse, pipelined requests, a request delivered one byte at a time,
a request with only headers (timeout → 408), 413 enforcement, redirect, autoindex.

**Why:** `[A-35]` — the auditor literally runs `cargo test` and all of it must pass.
The one-byte-at-a-time test is the strongest single proof that the incremental
parser and the one-read-per-event design actually cooperate.

### Task 9.2 — Manual audit script
**Do:** `tests/audit.sh` with a labelled `curl` per audit question:
```bash
curl -i http://127.0.0.1:8080/                                  # 200
curl -i http://127.0.0.1:8080/nope                              # 404
curl -i -X DELETE http://127.0.0.1:8080/static/x.html           # 405
curl -i -X POST --data "$(head -c 2000000 /dev/zero|tr '\0' a)" \
     http://127.0.0.1:8080/upload                               # 413
curl -i --resolve test.com:8080:127.0.0.1 http://test.com:8080/ # vhost
curl -i -X POST -F "file=@image.png" http://127.0.0.1:8080/upload
curl -s http://127.0.0.1:8080/uploads/image.png | sha256sum     # compare
curl -i http://127.0.0.1:8080/old-page                          # 301
printf 'GARBAGE\r\n\r\n' | nc 127.0.0.1 8080                    # 400, server alive
```
**Why:** `[A-9..22]`. Having these ready turns the audit from live improvisation into
a walkthrough, and each one is a regression test you can re-run after any change.

### Task 9.3 — Siege / stress
**Do:** Install siege. Run `siege -b 127.0.0.1:8080/empty.html` for 60s+; target
**≥ 99.5 %** availability. While it runs, watch `ls /proc/$(pgrep localserver)/fd | wc -l`
and RSS in `top`. Then `siege -b` against a CGI endpoint and against an upload endpoint.

**Why:** `[A-32]`, `[A-33]`, `[A-34]`, and the 99.5 % target. Expect the first run to expose
missing `SIGPIPE` handling, connections not closed on `Connection: close`, and fd
growth from unreaped CGI children. Fix, re-run, and record the numbers in the README.

### Task 9.4 — Leak and hanging-connection checks
**Do:** fd count before/after a siege run must return to baseline. `ss -tan | grep 8080`
should show no lingering `CLOSE_WAIT`/`ESTABLISHED` after clients stop. Run under
`valgrind --leak-check=full` once (mostly to have the output on hand) and confirm no
zombie processes (`ps --ppid $(pgrep localserver)`).

**Why:** `[A-33]`, `[A-34]`. `CLOSE_WAIT` accumulating is the specific signature of
"peer closed but I never deregistered the fd", i.e. Task 2.5 done wrong. Zombies are
the signature of missing `waitpid`.

### Task 9.5 — Browser pass
**Do:** In Chrome/Firefox devtools: load the static site, check every request is 200
with correct `Content-Type`; verify a 404 page renders; follow a redirect (check the
`Location` header and the 301 in the Network panel); browse a directory listing;
submit a form to CGI; upload a file; inspect the session cookie; confirm keep-alive
(`Connection: keep-alive`, connection reuse in the Network timing panel).

**Why:** The whole "Interaction with the browser" section (`[A-23..28]`). Browsers
send headers curl doesn't (`Accept-Encoding`, `Sec-Fetch-*`, conditional requests) —
this catches parser assumptions that `curl` never exercises.

### Task 9.6 — README
**Do:** Document build/run, the full config reference with an example of every
feature, the architecture (one thread, one epoll, connection state machine), the
CGI environment table, how to run the tests, and the siege results.

**Why:** The audit opens with "take the time to understand the project — looking into
the source code will help". A README that names the epoll design and the module map
makes every subsequent question faster to answer.

---

## Phase 10 — Audit rehearsal

### Task 10.1 — Prepare the code walkthrough
**Do:** Write `AUDIT_NOTES.md` with `file.rs:line` pointers for each functional
question:
- `server.rs` — the single `Poll::poll` call and the token dispatch (`[A-2..4]`).
- `connection.rs` — the single `read()` and single `write()` sites (`[A-5]`).
- `connection.rs` — the `io::Result` match arms, `WouldBlock` vs fatal (`[A-6]`).
- `server.rs` — `deregister` + remove-from-map on error (`[A-7]`).
- `cgi.rs` — pipes registered in the same Poll, proving no I/O bypasses epoll (`[A-8]`).

**Why:** `[A-1..8]` are all "show me in the source". A line-referenced cheat sheet is
the difference between "I think it's around here somewhere" and a clean pass.

### Task 10.2 — Deliberate misconfigurations
**Do:** Keep ready-made broken configs: `config.dup-port.yaml` (same host:port +
same server_name → must fail at startup with a clear message) and
`config.partial-bad.yaml` (three servers, one with a nonexistent root → the other two
must still serve).

**Why:** `[A-30]`, `[A-31]`. The second one is where the auditor asks *why* the server
should still work — your answer is the two-severity design from Task 1.3.

---

## Audit question → task mapping

| # | Audit question | Task |
|---|---|---|
| A-1 | How does an HTTP server work? | 2.2, 3.1, 9.6 |
| A-2 | Which I/O multiplexing function, how does it work? | 2.1, 2.2, 10.1 |
| A-3 | Only one select/epoll for reads and writes? | 2.2, 7.1 |
| A-4 | Why one epoll, and how achieved? | 2.2, 10.1 |
| A-5 | Only one read/write per client per epoll? | 2.4, 10.1 |
| A-6 | Return values of I/O functions checked? | 2.5 |
| A-7 | Client removed on socket error? | 2.5, 2.6 |
| A-8 | Is read/write ALWAYS via epoll? | 2.4, 5.1, 7.1 |
| A-9 | Single server, single port | 1.1, 2.1 |
| A-10 | Multiple servers, different ports | 1.1, 2.1 |
| A-11 | Multiple servers, different hostnames | 1.1, 4.1 |
| A-12 | Custom error pages | 1.1, 5.5 |
| A-13 | Client body limit | 1.1, 3.2 |
| A-14 | Routes taken into account | 4.2 |
| A-15 | Default file for a directory | 4.2, 5.4 |
| A-16 | Accepted methods per route (DELETE w/ and w/o permission) | 4.2, 5.3 |
| A-17 | GET works | 5.1 |
| A-18 | POST works | 5.2 |
| A-19 | DELETE works | 5.3 |
| A-20 | Wrong request, server still up | 2.7, 3.1, 9.2 |
| A-21 | Upload + download uncorrupted | 3.3, 5.2, 9.2 |
| A-22 | Sessions and cookies | 6.1, 6.2 |
| A-23 | Browser connects with no issues | 3.4, 9.5 |
| A-24 | Request/response headers correct | 3.4, 9.5 |
| A-25 | Wrong URL handled | 4.3, 5.5 |
| A-26 | Directory listing | 5.4 |
| A-27 | Redirected URL | 4.2 |
| A-28 | CGI with chunked and unchunked data | 3.2, 7.1, 7.3 |
| A-29 | Multiple ports and websites | 1.1, 2.1, 4.1 |
| A-30 | Same port configured twice → error | 1.3, 10.2 |
| A-31 | Common ports, one bad config → others still work | 1.3, 10.2 |
| A-32 | siege ≥ 99.5 % availability | 2.x, 9.3 |
| A-33 | No memory leak | 9.3, 9.4 |
| A-34 | No hanging connection | 2.6, 9.4 |
| A-35 | `cargo test` all passing | 9.1 |
| A-36 | Tests for HTTP request parsing | 3.5 |
| A-37 | Tests for configuration validation | 1.4 |
| A-38 | Tests for route matching | 4.4 |
| A-39 | + More than one CGI system | 7.4 |
| A-40 | + Admin dashboard / metrics | 8.1 |

---

## Suggested order of work

1. Phase 0 → 1 → 2 first: a server that accepts connections and replies a hardcoded
   `200 OK` through a correct event loop is worth more at the audit than a feature-rich
   server with a blocking loop. Phases 0–2 answer eight of the functional questions.
2. Phase 3 → 4 → 5: now the config actually drives behaviour; run `tests/audit.sh`
   after each task.
3. Phase 6, then Phase 7 (CGI is the hardest task — Task 7.1 will likely force
   refactors in `connection.rs`; leave room for that).
4. Phase 9 continuously, not at the end: run `siege` as soon as Phase 2 works, so
   fd-leak and timeout bugs surface while the code is still small.
5. Phase 8 and Task 7.4 (bonuses) only once the mandatory audit passes end to end.
