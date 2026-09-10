# Changelog

All notable changes to LocalServer are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries are grouped under `Added`, `Changed`, `Fixed`, `Removed`, `Security`.
Where a change satisfies an audit question, reference it as `[A-n]` using the
numbering in `TASKS.md`.

> **Conventions**
> 1. This file is updated with **every commit**. The entry goes under the
>    `Unreleased` heading and is staged in the *same* commit as the change it
>    describes — never as a separate follow-up commit.
> 2. Every entry states **what** changed and, on an indented `_Why:_` line,
>    **why** it was done. The what is recoverable from `git diff`; the reasoning
>    is not, and it is what the audit actually asks for.

## [Unreleased]

### Added — Interactive Visual Audit Dashboard & Studio (2026-09-10)

- **Interactive Visual Audit Dashboard** (`www/index.html`, `www/static/audit-console.js`, `www/static/style.css`):
  - Comprehensive tabbed browser interface on `/` featuring a live Visual Audit Suite, File Upload & SHA-256 Studio, CGI Lab, Session Manager, Routes/Vhosts Explorer, and Custom HTTP Request Console.
  - _Why:_ Allows evaluating and demonstrating all audit questions (`[A-9]` through `[A-40]`) with single-click browser tests, status code badges, latencies, RFC 9110 response headers, and formatted body previews without typing terminal `curl` commands.
- **Web Crypto SHA-256 Roundtrip Verification** (`www/static/audit-console.js`):
  - In-browser SHA-256 hashing via `crypto.subtle.digest` before upload and after download, providing side-by-side verification of binary integrity.
  - _Why:_ Provides visual proof of zero corruption during multipart form uploads (`[A-21]`).
- **Standard CORS Headers** (`src/http/response.rs`):
  - Emits `Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: *`, `Access-Control-Allow-Headers: *`.
  - _Why:_ Enables browser `fetch()` on port 8080 to test ports 8081 and 8082 without browser cross-origin blocks.
- **Virtual Host Testing Support (`X-Host`)** (`src/http/request.rs`):
  - Request parser checks `X-Host` header fallback when `Host` cannot be modified by browser `fetch()`.
  - _Why:_ Allows testing virtual host selection (`localhost` vs `test.com`) directly from the browser interface.
- **Zombie Process Elimination & Hangup Cleanup** (`src/cgi.rs`, `src/server.rs`):
  - `into_response_and_child()` passes `Child` to `Server.pending_reap` for non-blocking reaping via `try_wait()` and `waitpid(-1, WNOHANG)`.
  - `dispatch()` handles `event.is_read_closed()` to close sockets promptly on client hangup.
  - _Why:_ Guarantees 0 zombie processes and 0 file descriptor leaks under heavy concurrent stress testing (`[A-33]`, `[A-34]`).

### Fixed & Security — Adversarial Audit Hardening (2026-09-10)

- **HEAD on 405 + `/session` now omits body** (`src/connection.rs`):
  - `Resolution::MethodNotAllowed` and `session_demo` apply `.head_only(head_only)`.
  - _Why:_ RFC 9110 §9.3.2 — HEAD MUST NOT send a body; previously `curl -I` on `/readonly/*` or `/session` returned bytes, risking keep-alive desync. [A-24]
- **Non-blocking CGI teardown** (`src/cgi.rs`):
  - `kill()` and `Drop` use `try_wait()` (WNOHANG) only; reaping stays on the loop tick.
  - _Why:_ blocking `wait()` on the single thread stalls every client if a child is slow to die. [A-8, A-33]
- **`dispatch_cgi` panic isolation + readiness gating** (`src/server.rs`):
  - `dispatch_cgi` wrapped in `catch_unwind` → 502 + close (mirrors `dispatch`); stdout/stderr gated on `is_readable()||is_error()`; client dispatch closes on `is_error()/is_write_closed()`.
  - _Why:_ a CGI bug must cost one client, not the process; spurious error events must not trigger reads. [A-20]
- **Request parser hardening** (`src/http/request.rs`):
  - `Transfer-Encoding` requires `chunked` final (else 501); `Content-Length` requires `1*DIGIT` (rejects `+5`); `Host: name:garbage-port` rejected with 400.
  - _Why:_ closes smuggling/framing disagreements where lenient vs strict parsers diverge. [A-20, A-36]
- **Upload symlink guard** (`src/handlers/mod.rs`):
  - `write_file` refuses `403` when the destination is a symlink.
  - _Why:_ prevents `uploads/link -> /outside` redirecting an upload outside the configured directory. [A-21]
- **Filename `a..b` allowed** (`src/http/multipart.rs`):
  - `sanitize_filename` only rejects `.`/`..` themselves and leading-dot hidden files, not `..` substrings.
  - _Why:_ `a..b` is a harmless single component; over-rejection breaks legit uploads. [A-21]
- **Lint coverage extended**: `#![deny(clippy::unwrap_used, expect_used)]` added to `config/*`, `main.rs`, `http/mod.rs`, `utils/{mime,mod}.rs`.
  - _Why:_ makes "no panics on the request path" enforced rather than intended. [A-20]
- **6 new regression tests** (246 unit + 16 integration = 262 total): non-final chunked, signed CL, bad Host port, `a..b` filename, symlink upload refusal.

### Added

- **Phase 8.1 / [A-40] Bonus — Admin Dashboard & Metrics Endpoint** (`src/metrics.rs`, `src/handlers/admin_page.rs`, `src/server.rs`, `src/connection.rs`):
  - In-memory lock-free telemetry tracking uptime, active/total connections, requests by HTTP method, responses by status code class (1xx..5xx), bytes received and sent, CGI child process metrics (spawned, failed, timed-out), active sessions, and live open file descriptor count parsed from `/proc/self/fd`.
  - Served at `GET /admin` with a self-refreshing HTML dashboard (3s meta refresh, dark theme matching `/session`, statistics cards and method/status breakdown tables) and at `GET /admin/metrics.json` returning clean machine-readable JSON.
  - _Why:_ `[A-40]` bonus requirement. Doubles as live debugging and verification proof during stress tests and audits to demonstrate 0 socket and file descriptor leaks in real time.

- **Phase 9.1 / [A-35] — Automated Live Integration Test Suite** (`tests/integration.rs`):
  - 16 end-to-end integration tests driving a live background `localserver` instance using raw `std::net::TcpStream`.
  - Comprehensive coverage: static file serving & MIME validation, custom 404 pages, accepted HTTP methods (`DELETE` 204 vs 405 `Allow`), `client_max_body_size` 413 rejection on 1K limit port 8082, virtual host discrimination on port 8080 (`Host: localhost` vs `test.com`), binary multipart upload and roundtrip SHA256 integrity check, 301 Moved Permanently redirects, directory autoindex HTML generation, HTTP pipelined requests in a single TCP write, incremental one-byte-at-a-time trickle parsing, cookie and session visit counter increment, CGI execution for Python and Perl, CGI 504 gateway timeout, malformed garbage request survival (400 Bad Request), admin metrics endpoint validation, and security hardening edge cases (percent-encoded route matching and redirect HTML escaping).
  - _Why:_ `[A-35]` requires automated test suite proving all protocol, routing, and concurrency guarantees live over real TCP sockets.

- **Phase 9.2 / [A-9..28] — Automated Audit Walkthrough Script** (`tests/audit.sh`):
  - Self-contained executable bash script exercising all functional audit questions `[A-9]` through `[A-28]` against a live server.
  - 20 automated checks testing port binding, virtual hosting, custom error pages, body limits, route routing, default index, method permissions, GET/POST/DELETE, garbage request tolerance, binary upload/download integrity, session cookies, browser compatibility headers, autoindex, 301 redirects, and CGI streaming.
  - _Why:_ Provides a deterministic, single-command validation harness that outputs colorized `[PASS]` lines with exact audit question references.

- **Phase 9.3 & 9.4 / [A-32..34] — Async Stress & Leak Verification Suite** (`tests/stress.py`):
  - High-performance Python 3 `asyncio` stress harness simulating `siege -b -c 25 -t 10s`.
  - Concurrently sends thousands of requests across static endpoints, sessions, and CGI.
  - Automatically measures availability (>= 99.5% target), monitors `/proc/$PID/fd` before and after execution to assert 0 FD leak, checks `ss -tan` to assert 0 lingering `CLOSE_WAIT` connections, and checks `ps --ppid $PID` to assert 0 zombie processes.
  - _Why:_ Proves stability under heavy concurrency without external dependencies or root permissions.

- **Phase 10.1 / [A-1..40] — Comprehensive Audit Guide & Cheat Sheet** (`docs/AUDIT_GUIDE.md`, `docs/AUDIT_NOTES.md`):
  - In-depth preparation guide answering all 40 questions from `docs/audit.md`, including architectural explanations, step-by-step code paths, RFC citations, curl verification commands, and defensive audit strategies.
  - Compact quick-reference table in `docs/AUDIT_NOTES.md` mapping each question `A-1..40` directly to exact source file and line numbers.
  - _Why:_ Fully prepares the student to defend the architecture, walk through the codebase, and answer any theoretical or practical question posed by the auditor.

- **Phase 10.2 / [A-30, A-31] — Deliberate Misconfiguration Testbeds** (`config.dup-port.yaml`, `config.partial-bad.yaml`):
  - `config.dup-port.yaml`: Verifies fatal startup abort (exit code 2) when duplicate ports share server names or default status.
  - `config.partial-bad.yaml`: Verifies two-severity resilience where a server with an invalid root is dropped with a warning, while two valid server blocks on the shared listener continue binding and serving cleanly.
  - _Why:_ Specifically required by audit questions `[A-30]` and `[A-31]`.

### Fixed & Security

- **[A-5] Strict One-Operation-Per-Poll Invariant** (`src/server.rs`):
  - Updated event dispatch from sequential `if is_readable() ... if is_writable()` to `if is_readable() ... else if is_writable()`.
  - _Why:_ Guarantees that even if `mio` returns both readable and writable readiness bits for a socket in a single `epoll_wait` event, the server executes at most one socket syscall per client per event loop tick. Remaining readiness is serviced in the next iteration via edge-triggered re-arming.

- **Reflected HTML Injection in 301 Directory Redirects** (`src/connection.rs`):
  - Escaped the `Location` URI in the 301 Moved Permanently HTML response body using `crate::handlers::escape_html`.
  - _Why:_ Prevented potential HTML injection via crafted unescaped request paths when auto-redirecting missing trailing slashes.

- **Percent-Encoded Route Prefix Matching Bypass** (`src/router.rs`, `src/utils/path.rs`):
  - Added percent-decoding to request paths prior to route prefix matching and introduced `resolve_segments_under` to prevent double percent-decoding during file segment resolution.
  - _Why:_ Neutralized a route match bypass where requests like `/%75ploads/file.txt` evaded route configurations while accessing the underlying directory.

- **Raw Upload Timestamp Collision Hardening** (`src/handlers/mod.rs`):
  - Upgraded raw stream upload auto-naming from seconds to nanosecond precision (`as_nanos()`).
  - _Why:_ Prevents filename overwrites when multiple clients upload raw stream payloads within the same one-second epoch window.

- **Phase 7.1 — non-blocking CGI** (`src/cgi.rs`, `src/server.rs`): scripts run
  as child processes whose stdin, stdout and stderr are set `O_NONBLOCK`,
  wrapped in `mio::unix::SourceFd` and registered in the **same `Poll`** as
  every socket. The body is pumped in one write per writable event, output
  collected one read per readable event.
  _Why:_ this is the whole of `[A-8]`. The obvious implementation —
  `wait_with_output()` — blocks until the script exits, which with one thread
  freezes every other client for the script's entire runtime, and fails the
  question outright since a pipe is a descriptor and reading it is reading.
  Demonstrated live: while `slow.py` sleeps, `/`, a stylesheet, a directory
  listing, `/session` **and a second CGI script** were all served in 1–27 ms.

- **Phase 7.2 — the CGI/1.1 environment**: `GATEWAY_INTERFACE`,
  `REQUEST_METHOD`, `QUERY_STRING`, `SCRIPT_NAME`, `SCRIPT_FILENAME`,
  `PATH_INFO`, `PATH_TRANSLATED`, `DOCUMENT_ROOT`, `CONTENT_TYPE`,
  `CONTENT_LENGTH`, `SERVER_*`, `REMOTE_*` and every request header as `HTTP_*`.
  _Why:_ `PATH_INFO` is the **extra path after the script name**, as RFC 3875
  defines it — `/cgi-bin/app.py/users/7` gives the script `app.py` and
  `PATH_INFO=/users/7`. The subject's phrase "use PATH_INFO to define full
  paths" is answered by `SCRIPT_FILENAME` and `PATH_TRANSLATED`, which carry the
  absolute filesystem paths; making `PATH_INFO` itself a filesystem path would
  break any real CGI script. Both are set, so either reading is served.

- **The child's working directory is the script's own directory.**
  _Why:_ "ensure correct relative path handling" — a script doing
  `open("data.txt")` must find the file next to itself, not next to wherever the
  server was started. `cgi-bin/data.txt` exists so `hello.py` can prove it.

- **The child's environment is cleared before it is populated.**
  _Why:_ a script should see the request, not whatever happened to be exported
  in the shell that launched the server. `PATH` is restored because an
  interpreter generally needs one.

- **Phase 7.3 — CGI response translation**: the script's own header block is
  parsed off the top of stdout, `Status:` sets the response code without being
  forwarded, and a `Location:` with no explicit status becomes a 302 per
  RFC 3875.
  _Why:_ `Connection`, `Transfer-Encoding` and `Content-Length` from a script
  are **discarded**. Framing belongs to the server: honouring a script's
  `Content-Length: 999` when it wrote 5 bytes would desynchronise the byte
  stream for the next request on that keep-alive connection. Output is buffered
  to a 1 MB cap so the real length is always known, which keeps `HEAD` honest
  and avoids chunking a response whose size is not in doubt.

- **A 10-second CGI deadline**, shorter than the 30-second connection idle
  timeout, killing the child and answering 504.
  _Why:_ without it one infinite loop is a permanent process and three permanent
  descriptors (`[A-33]`, `[A-34]`). Ordering the two timeouts this way means a
  runaway script is reported as a gateway timeout rather than decaying into a
  stalled connection. Verified: `slow.py` answers 504 after 10.1 s.

- **Children are reaped with `try_wait` (`waitpid(WNOHANG)`) from the loop
  tick, and killed on `Drop`.**
  _Why:_ an unreaped child is a zombie, which reads at the audit as a process
  leak; and a client that gives up mid-script must not leave the script running.
  Measured after 30 CGI requests, a stress run and an aborted request: **0
  children, 0 zombies, descriptors back to the 7-descriptor baseline**.

- **Phase 7.4 — `cgi-bin/hello.py`** (echoes the environment, the body and a
  relative file read), **`form.py`** (reads a POSTed body), **`slow.py`** (proves
  the timeout), and **`info.pl`** — a second CGI system in Perl.
  _Why:_ `[A-39]`. Adding Perl was **a config change only** — one `.pl` line in
  the route's `cgi` map — because the interpreter is keyed by file extension and
  the server knows nothing about any language. That is the thing worth showing
  an auditor, more than the script itself.

- **26 new tests**: the CGI environment including the `PATH_INFO` /
  `PATH_TRANSLATED` distinction, `HTTP_*` mangling, response translation,
  `Status:` handling, `Location:` inference, refusal of framing headers, and
  absolute script paths. 221 in total.

### Fixed

- **CGI script paths were resolved twice.** The working directory is set to the
  script's directory, so a *relative* script path — which is what a config
  relative to `config.yaml` produces — was resolved against it again:
  `cgi-bin/hello.py` became `cgi-bin/cgi-bin/hello.py` and every script answered
  an empty 200.
  _Why:_ paths are now canonicalised when the spec is built, before the child is
  spawned. There is a test pinning it.

- **Bodies larger than the pipe buffer stalled the script.** The third instance
  of the same edge-triggered problem, now on pipes: after one write the pipe was
  often still writable with no state transition, so `EPOLLET` reported nothing
  more and a 200 KB body hung until the CGI deadline turned it into a 504.
  _Why:_ pipes are now re-armed on partial progress, exactly as sockets are. The
  same fix covers output larger than one read. Verified: 100 KB, 200 KB and
  900 KB bodies all arrive intact through both `Content-Length` and chunked
  framing.


- **Phase 6.1 — cookie utilities** (`src/utils/cookie.rs`): `Cookie:` header
  parsing and a `SetCookie` builder covering `Path`, `Max-Age`, `HttpOnly`,
  `Secure` and `SameSite`.
  _Why:_ the two grammars are deliberately separate types. A request's `Cookie`
  header is a flat `name=value; name=value` list with no attributes — a client
  sends back only the pairs, never the `Path` or `HttpOnly` it was given — while
  `Set-Cookie` carries one pair plus its attributes. Treating them as one format
  is a common source of confusion.

- **Cookie values that could inject headers are rejected, not escaped.**
  _Why:_ a value containing `;` could smuggle in an extra attribute, and one
  containing CRLF an entire extra response header. Refusing is a decision the
  caller can see; escaping quietly changes what was asked for.

- **Defaults chosen rather than inherited**: `HttpOnly` and `SameSite=Lax` on,
  `Secure` deliberately off.
  _Why:_ `HttpOnly` keeps the session id out of `document.cookie`, so a
  scripting bug in a served page cannot read it. `SameSite=Lax` stops it riding
  along on cross-site form posts. `Secure` is off because this server speaks
  plain HTTP — a `Secure` cookie would simply never be sent back, and the
  feature would look broken.

- **Phase 6.2 — in-memory session store** (`src/utils/session.rs`) with sliding
  expiry, a 30-minute TTL, and ids of 32 bytes from `/dev/urandom`, hex-encoded.
  _Why:_ `[A-22]`. A guessable id **is** a session hijack — anyone who can
  predict one is logged in as that user — so neither the clock nor a counter is
  acceptable, and the kernel already has a CSPRNG. If `/dev/urandom` cannot be
  read the store refuses to invent a weak id: it stops issuing sessions, says so
  once, and the site keeps working without them. Serving without sessions is
  better than serving forgeable ones.

- **No locking, on purpose.** The store is a `HashMap` behind
  `Rc<RefCell<_>>`, shared by every connection.
  _Why:_ this is the honest answer to the subject's "use thread-safe data
  structures" tip and worth saying out loud at the audit: there is exactly one
  thread, so a `Mutex` would buy nothing but syscalls and a deadlock risk, and
  `Arc` would pay for atomic refcounting that is never contended. The
  single-threaded design *is* the synchronisation. `RefCell` still enforces the
  borrow rules at runtime, and every borrow on the request path uses
  `try_borrow_mut` rather than `borrow_mut`, because a nested borrow would panic
  and nothing on the request path may panic.

- **A cap of 10,000 sessions**, sweeping expired entries before refusing.
  _Why:_ every request without a valid cookie creates a session, so a client
  that ignores cookies would otherwise grow the map for as long as it kept
  asking. Measured under `siege`: 81,555 requests left **33** sessions, because
  siege keeps a cookie jar per simulated user and so behaves like a browser —
  but the cap is what makes the pathological client harmless rather than
  hopeful.

- **Expired sessions swept from the event loop tick, once a minute** rather than
  per request.
  _Why:_ sweeping walks the whole map, so doing it per request would make each
  one cost O(sessions), and doing it every 500 ms tick would keep an idle server
  busy for nothing. Expiring a minute late costs nothing: the lookup checks
  expiry itself, so a stale session is never handed out — it just occupies
  memory slightly longer.

- **`/session` demo page** showing the id, a visit counter, a sliding expiry
  countdown, a sign-in form and a sign-out link. Linked from the index page.
  _Why:_ sessions are invisible by nature, and "show me a working session and
  cookie system" is answered by opening devtools → Application → Cookies. The
  page makes the same thing visible without devtools — reload it and the counter
  climbs. Sign-in redirects with **303 rather than 302** so a reload afterwards
  does not re-post the form.

- **20 new tests**: cookie parsing and rendering, header-injection refusal, id
  shape and uniqueness, expiry and sliding renewal, the store cap, form parsing,
  and that a stored name is HTML-escaped before it reaches the page. 204 in
  total.

### Changed

- Session handling wraps the whole request path rather than living inside a
  handler, and `Set-Cookie` is attached only when a session is newly issued.
  _Why:_ doing it in one place means every response carries the session
  consistently, error pages included. Re-sending an unchanged cookie on every
  response would waste bytes on every asset a page pulls in — verified: the
  first request to `/session` carries `Set-Cookie`, the next three do not, and
  the visit counter still climbs 1, 2, 3.
- A cookie whose value is not shaped like an id this server issues (64 hex
  characters) is treated as absent.
  _Why:_ it stops a client probing the session map with arbitrary strings, and
  makes a mangled cookie from another application on the same host behave as no
  cookie at all rather than as a failed lookup.


- **Phase 5.1 — static file serving** (`src/handlers/mod.rs`) with
  `Content-Length` and `Last-Modified` from the file's metadata, `Content-Type`
  from the MIME table, and `HEAD` returning the length with no body.
  _Why:_ `[A-17]`. Verified byte-exact: a 10 MB download and a 300 KB upload
  both round-trip with matching SHA-256.

- **Bodies are streamed, not slurped**: a file larger than 64 KB stays on disk
  and is pulled one chunk per writable event.
  _Why:_ `[A-33]`. Reading a large download into a `Vec` first would spike
  memory by the file's size per concurrent client and stall the single thread
  while it happened. Measured: serving a 10 MB file moved the server's RSS by
  128 KB. Regular-file I/O is the one thing not driven by epoll, and that is not
  a shortcut — `epoll` reports a regular file as always ready, so there is
  nothing to wait for; chunking is what keeps a big download from monopolising
  the loop, for the same reason as one socket read per event.

- **Files that fit in one chunk are sent inline with their headers.**
  _Why:_ streaming a 1 KB stylesheet costs three syscalls and two epoll re-arms
  to move a few hundred bytes, and small files are most of a static site's
  traffic. Memory is bounded either way (never more than one chunk). Worth
  3.4× on the audit's own benchmark: `siege -b` on `/empty.html` went from 816
  to **2805 trans/sec**, and `/index.html` from 276 to **989**.

- **Phase 5.2 — uploads** accepting both shapes the audit uses:
  `multipart/form-data` (a browser form or `curl -F`) and a raw body
  (`curl --data-binary`). Answers `201` with a `Location`.
  _Why:_ `[A-18]`, `[A-21]`. A filename that sanitises to nothing (`..`, a
  dotfile, control characters) is **refused with 400 rather than silently
  renamed**, so a client never believes it stored something it did not; a
  traversing name keeps only its final component and lands inside the upload
  directory, which has its own test.

- **Phase 5.3 — DELETE** answering `204`, with `404` for a missing file and
  `403` for a directory.
  _Why:_ `[A-19]`. A 204 carries no body and, per RFC 9110, no
  `Content-Length` either — some clients read one as a promise of bytes that
  never arrive and hold the connection open. Demonstrated against `[A-16]`:
  `DELETE /uploads/note.txt` → 204 then 404, while `DELETE /readonly/...` on
  the same directory → 405 with `Allow: GET` and the file survives.

- **Phase 5.4 — directory listings** with sizes, modification times, a parent
  link (except at the route root, where it would leave the served tree), hidden
  files omitted, and directories sorted first.
  _Why:_ `[A-26]`. Names are **HTML-escaped for the link text and
  percent-encoded for the href** — two encodings for two contexts. Using one for
  both is the classic bug and lets a file named `<script>.txt` inject markup
  into the page; there is a test for exactly that.

- **Phase 5.5 — error pages** (`src/error.rs`): the page configured for that
  status, else a built-in page compiled into the binary. Covers 400, 403, 404,
  405, 408, 413, 500, 501, 505 and more.
  _Why:_ `[A-12]`. The fallback is load-bearing, not a nicety: if a configured
  page is missing or was deleted after startup validation approved it,
  generating the error response would itself fail — and answering *that* with
  another error page recurses. A page that needs no filesystem access and cannot
  fail terminates the chain. A configured page larger than 256 KB is also
  refused, so an ordinary 404 cannot be turned into a large allocation.
  Errors raised **before** a request exists (a malformed request line, a
  timeout) use the listener's default server, so a custom 400 page applies even
  to a request too broken to name a host.

- **26 new tests**: file serving and streaming thresholds, HEAD, upload
  round-trip with binary content, unsafe and traversing filenames, delete
  semantics, listing escaping and encoding, size formatting, error page
  fallback. 178 in total.

### Fixed

- **Streamed responses stalled after the headers.** The same edge-triggered
  problem as the read side, in the other direction: a *complete* write leaves
  the socket writable with no state transition, so `EPOLLET` reports nothing
  further and the queued body was never sent — clients received correct headers
  and then hung waiting for a `Content-Length` worth of bytes.
  _Why the rule differs by case:_ a **partial** write means the socket buffer
  filled, and the peer draining it produces a genuine writable edge, so no
  re-arm is needed. A **complete** write with more still to send has no such
  edge coming, so it re-arms. Small in-memory responses hid this completely —
  they fit in a single write, which is why every phase up to now passed.

- **`Location` on an upload doubled the filename** when the URL already named
  the file: `POST /uploads/note.txt` answered
  `Location: /uploads/note.txt/note.txt`.
  _Why:_ the header was built by appending the stored name to the request path,
  which is only right when the path names a directory. Caught by a unit test
  before it reached the live checks.


- **Phase 4.1 — virtual host selection** (`src/router.rs`): `Host` (port
  stripped, lowercased) matched against `server_names`, falling back to the
  `is_default` server for that address and then to the first declared.
  _Why:_ `[A-11]`. Verified live on one socket: `curl http://127.0.0.1:8080/`
  serves `www/index.html` while
  `curl --resolve test.com:8080:127.0.0.1 http://test.com:8080/` serves
  `www/site2/index.html`. An unmatched `Host` falls back rather than erroring,
  which is what makes a plain IP request work at all.

- **Phase 4.2 — longest-prefix route matching** with a deliberate resolution
  order: redirect, then method, then path sanitising, then CGI, then directory
  rules, then file.
  _Why:_ the audit asks about this order. A **redirect wins over the method
  check** because the route is not serving the resource — `/old-page` redirects
  whether or not the file exists and whatever the method. **405 precedes 404**
  because whether the resource exists is not the question being answered, and
  revealing existence to a client that may not use that method leaks
  information. **CGI precedes the directory rules** so a script is executed
  rather than offered as a download. A prefix only matches at a segment
  boundary, so `/upload` cannot capture `/uploads` and serve it from the wrong
  root.

- **`Allow` header on every 405**, listing the route's configured methods.
  _Why:_ required by RFC 9110, and `[A-16]` tests DELETE with and without
  permission. `config.yaml` ships `/uploads` (GET, POST, DELETE) and
  `/readonly` (GET) over the same directory, so the pair is a two-command
  demonstration: `DELETE /readonly/x` answers `405` with `Allow: GET`.

- **Directory rules**: configured `index` if present, else a listing when
  `autoindex` is on, else **403 rather than 404**.
  _Why:_ `[A-15]`. The directory does exist — the client is simply not allowed
  to enumerate it, and 404 would be a lie.

- **301 redirect for a directory requested without a trailing slash**,
  preserving the query string.
  _Why:_ without it every relative link on the served page resolves one level
  too high, so browsing a nested directory breaks in a real browser while
  looking fine under `curl`.

- **`PATH_INFO` splitting for CGI**: the first path segment whose extension is
  registered becomes the script, everything after it is `PATH_INFO`.
  _Why:_ the subject requires `PATH_INFO`. Resolving the whole URL to one file
  instead would 404 on `/cgi-bin/app.py/users/7`, which is a legitimate CGI
  request.

- **Phase 4.3 — path sanitising** (`src/utils/path.rs`): percent-decode, reject
  `%00`, normalise `.`/`..`, then verify with `canonicalize` that the result is
  still under the route root.
  _Why:_ the order is the whole point. A normaliser that runs **before**
  decoding never sees the `..` hidden in `%2e%2e%2f`. `canonicalize` cannot be
  the only defence because it fails outright on a path that does not exist, so a
  traversal aimed at a missing file would skip the check entirely; lexical
  normalisation makes the path safe before the filesystem is consulted, and
  `canonicalize` then catches what lexical work cannot — a **symlink inside the
  root pointing outside it**, which has its own test. `%00` is refused because a
  NUL truncates the path inside a syscall, so the validator and the kernel would
  disagree about which file is meant.

- **Phase 4.4 — 35 new tests**: virtual host selection and fallback, longest
  prefix, segment-boundary matching, the full resolution order, 405 with
  `Allow`, index/autoindex/403, trailing-slash redirect, CGI extension and
  `PATH_INFO`, traversal in three encodings, symlink escape, upload and delete
  resolution. 152 tests in total.

### Fixed

- **Large request bodies deadlocked the connection.** `mio` registers
  descriptors **edge-triggered** (`EPOLLET`), which reports a socket once when
  data arrives and stays silent afterwards. With one read per event the bytes
  already sitting in the socket buffer were never announced again: the server
  waited for an event that would not come while the client waited for its window
  to drain. Small requests hid it completely — every test up to Phase 3 fitted
  in a single 16 KB read.
  _Why the fix is re-arming rather than draining:_ the obvious repair is to read
  until `WouldBlock`, which is exactly the behaviour `[A-5]` asks about and this
  design rejects — one client could then monopolise the single thread. Instead,
  when a read fills the buffer the connection asks the server to re-register it;
  `epoll_ctl(EPOLL_CTL_MOD)` on an edge-triggered descriptor re-reports
  readiness if data remains, so the next `poll()` returns this connection again.
  One read per event is preserved and every other client still gets its turn in
  between.

- **A declared `Content-Length` was only checked against the listener's limit,**
  never against the stricter one applied after the virtual host was known: a
  700 KB body reached a server configured for 512 KB and was answered by the
  router instead of being refused.
  _Why:_ `set_max_body` narrows the limit once `Host` has been read, but the
  length check only ran while parsing headers. It is now re-checked as the body
  is consumed, so `[A-13]` behaves the same for both framings — verified live:
  700 KB to `test.com` (512 KB limit) answers 413 whether sent with a
  `Content-Length` or chunked, while the same body to `localhost` (1 MB) is
  accepted.


- **Phase 3.1 — incremental request parser** (`src/http/request.rs`): request
  line, headers (case-insensitive, order-preserving, repeat-aware), version,
  and a `Headers::has_token` for comma-separated list headers. Reports
  `NeedMore` / `Complete` / `Failed(status)`.
  _Why:_ the loop reads once per event, so a request arrives in arbitrary
  fragments — a header split mid-word, a body dribbled a byte at a time. Keeping
  **incomplete** distinct from **invalid** is what lets one-read-per-event
  (`[A-5]`) and correct parsing (`[A-36]`) coexist; treating a short read as an
  error would break every client on a slow link. There is a test that feeds a
  whole request one byte at a time.

- **Rejections beyond plain syntax errors**, each with the status the audit
  expects: `Content-Length` together with `Transfer-Encoding` → 400, conflicting
  duplicate `Content-Length` → 400, whitespace before a header colon → 400,
  obsolete line folding → 400, missing or repeated `Host` on HTTP/1.1 → 400,
  unknown version → 505, unimplemented transfer coding → 501, over-long header
  block → 431, over-long request line → 414.
  _Why:_ the first two are the classic request-smuggling setup — two framings on
  one request, where servers that disagree about where the body ends can be made
  to see different requests. The rest are cases where a lenient parser and a
  strict one disagree, which is exactly where security bugs live. 505 rather
  than 400 for `HTTP/2.0`, and 501 rather than 400 for an unknown method,
  because those requests are well-formed — the server simply does not implement
  them.

- **Phase 3.2 — chunked codec** (`src/http/chunked.rs`): incremental decoder
  (size line, data, trailers, chunk extensions ignored) and an encoder for
  responses of unknown length.
  _Why:_ `[A-28]` asks about both directions. A chunked body cannot announce its
  size up front, so `client_max_body_size` has to be enforced **while decoding**
  rather than checked beforehand — a declared `Content-Length` over the limit is
  refused before a single body byte is buffered, which is the difference between
  answering 413 and buffering a 2 GB upload first. Tested by splitting the same
  stream at every possible byte position and checking all decode identically.

- **Phase 3.3 — multipart/form-data parser** (`src/http/multipart.rs`) with
  filename sanitising.
  _Why:_ `[A-21]` uploads a file and downloads it again to check it was not
  corrupted. Parts are located by byte search and copied verbatim — nothing
  converts a body to a `String`. The CRLF before a boundary belongs to the
  delimiter, not the file; getting that wrong appends two stray bytes to every
  upload and is invisible on text files but corrupts every binary one, so there
  is a test with NULs, high bytes and embedded CRLF. `sanitize_filename` keeps
  only the final path component and refuses `..`, hidden files and control
  characters outright rather than trying to rewrite them.

- **Phase 3.4 — response builder** (`src/http/response.rs`) always emitting
  `Content-Length` (or honouring `Transfer-Encoding`), `Date`, `Server` and an
  explicit `Connection`, with `HEAD` support that keeps the length but drops the
  body.
  _Why:_ `[A-24]`. A browser is far less forgiving than `curl`: a missing or
  wrong `Content-Length` makes it hang waiting for bytes that never arrive,
  which reads at the audit as a broken server.

- **RFC 9110 `Date` formatting from scratch**, using Howard Hinnant's
  `civil_from_days`.
  _Why:_ no date crate is needed for a dozen lines of arithmetic, and the
  IMF-fixdate format is fixed by the spec rather than locale-dependent. Verified
  against the specification's own example (`Sun, 06 Nov 1994 08:49:37 GMT`) plus
  leap days and the 400-year century rule.

- **`Expect: 100-continue` handling** via a new `AfterWrite::ResumeBody` state:
  the interim response is queued and flushed through the normal writable path,
  then reading resumes.
  _Why:_ a client that announces the expectation waits a second before sending
  its body anyway, taxing every upload. Handling it through the event loop's
  own write path rather than an opportunistic direct `write` keeps the rule that
  all I/O goes through epoll (`[A-8]`) intact.

- **Pipelined request handling**: after a response is flushed the parser is
  re-driven over bytes already buffered.
  _Why:_ a client may send two requests in one packet. Waiting for another read
  that never comes would stall it until the idle timeout.

- **Phase 3.5 / MIME table** (`src/utils/mime.rs`), defaulting to
  `application/octet-stream`.
  _Why:_ guessing `text/plain` for unknown bytes makes a browser try to render
  binary files and invites content sniffing.

- **117 tests** (up from 49): 30 request-parser, 12 chunked, 10 response,
  10 multipart, 6 MIME, plus the existing config and I/O ones.

### Changed

- `connection.rs` now drives the real parser instead of scanning for
  `\r\n\r\n`, and builds responses with the response builder. Malformed
  requests answer their true status instead of 200.
  _Why:_ `[A-20]` is only genuinely satisfied once a wrong request produces a
  wrong-request *status*, not merely a server that survives it. Verified live:
  garbage → 400, `Host : x` → 400, missing `Host` → 400, `HTTP/2.0` → 505,
  `BREW` → 501, 5000-byte body on the 1 KB port → 413.
- `Connection::new` takes the listener's body limit; `server.rs` computes it per
  listener as the maximum across the servers sharing that address.
  _Why:_ which virtual host a request belongs to is unknown until its `Host`
  header arrives, so the limit applied while reading is the most permissive one
  on that address. `RequestParser::set_max_body` then narrows it to the selected
  server in Phase 4. Using the maximum first is the safe direction: it never
  rejects a request some virtual host would have allowed.


- **Phase 2.1/2.2 — the event loop** (`src/server.rs`): one `mio::Poll`, one
  `Events` buffer, one connection map. Listener tokens occupy
  `[0, 1<<20)` and connection tokens start above it, so dispatch is a single
  comparison. `poll()` is called with a 500 ms tick.
  _Why:_ `epoll` over `select` because `select` is O(watched fds) per call,
  rebuilds its fd sets every time, and cannot watch a descriptor above
  `FD_SETSIZE` (1024) — which a server under `siege` reaches. **One** instance
  rather than several because with two, a descriptor could become ready in the
  first while the thread is blocked in the second's `wait()`; the only escapes
  are spinning or a second thread, and the subject allows neither
  (`[A-2]`..`[A-4]`). The tick is what lets timeouts be enforced when no traffic
  arrives at all.

- **Phase 2.3 — accept loop** that drains the backlog until `WouldBlock`, sets
  `TCP_NODELAY`, and registers each client as `READABLE`.
  _Why:_ this is the one place a drain loop is correct — every `accept()`
  returns a *different* descriptor, so looping serves many clients rather than
  favouring one. It is not the "one read per client" rule, which is about a
  single client's socket. A failing `accept` (EMFILE, ECONNABORTED) logs and
  returns instead of propagating: existing clients are unaffected (`[A-20]`).

- **Connection ceiling with listener throttling** (512 connections): at capacity
  the listeners are deregistered from the poll and re-registered when a
  connection closes.
  _Why:_ without a ceiling, fd exhaustion makes `accept` fail with `EMFILE`
  while the listener stays readable — the loop then spins at 100% CPU retrying a
  call that cannot succeed. Deregistering parks new connections in the kernel
  backlog, which is what a backlog is for.

- **Phase 2.4 — exactly one `read()` or one `write()` per client per event**
  (`src/connection.rs`), with the interest re-registered on every state change
  and never set to both directions at once.
  _Why:_ `[A-5]` asks this literally. Draining one client until `WouldBlock`
  lets a fast uploader or a slow-loris hold the single thread while everyone
  else waits. One syscall per event is fair round-robin, and level-triggered
  registration re-reports the remainder on the next `poll()`, so stopping early
  loses nothing.

- **Phase 2.5 — `classify_read` / `classify_write`**, collapsing every I/O
  return value into `Progress` / `PeerClosed` / `WouldBlock` / `Failed`.
  _Why:_ `[A-6]` and `[A-7]`. The distinction that matters is that `WouldBlock`
  and `Interrupted` are **not** errors — dropping a client because its socket
  had nothing to give is the classic bug in this project, and there is a test
  named for it. The two classifiers differ deliberately: `Ok(0)` from a read is
  end-of-stream, but `Ok(0)` from a write means the socket accepted nothing
  while claiming to be writable, which would spin the loop if treated as
  progress, so it is reported as a failure.

- **Phase 2.6 — two deadlines per connection**: idle (30 s without bytes moving)
  and total request duration (60 s), swept every loop iteration. A partial
  request gets a 408; an idle keep-alive connection is closed silently.
  _Why:_ `[A-34]`. A client that sends `GET / HTTP/1.1\r\n` and stops would
  otherwise hold a descriptor forever. The silent close for an idle keep-alive
  is deliberate: an unused connection slot expiring is normal, not an error, and
  answering 408 to a client that is not mid-request would be wrong. Verified
  live: 408 after 30.1 s, quiet close after 29.8 s.

- **Phase 2.7 — never crash.** `SIGPIPE` ignored at startup, per-connection
  handling wrapped in `catch_unwind`, `#![deny(clippy::unwrap_used,
  clippy::expect_used)]` on `server.rs` and `connection.rs`, and `EINTR` from
  `poll()` treated as "poll again" rather than fatal.
  _Why:_ `SIGPIPE` specifically because a browser closing a tab mid-response
  makes the next write raise it, and its default action **terminates the
  process** — the server would die on entirely normal client behaviour. The
  `catch_unwind` turns a logic bug into one dropped client instead of a dead
  server. The lint denial makes the "no panics on the request path" rule
  enforced rather than merely intended.

- **Graceful bind failures**: a listener that cannot bind is reported and
  skipped; startup fails only if none bound.
  _Why:_ same reasoning as the Skip severity in config validation (`[A-31]`) —
  one unusable port should not deny service on the others. Confirmed
  accidentally during testing when a stray instance held the ports: three
  warnings, then a clean exit 2.

### Changed

- `main.rs` now binds the listeners and enters the event loop after printing the
  configuration summary.
  _Why:_ `run()` returns only when `poll()` itself fails, which is genuinely
  unrecoverable — without readiness notification the process cannot serve
  anyone. Every per-client failure is handled inside the loop and never reaches
  `main`.

- Collapsed two nested conditionals and a `len() > 0` flagged by clippy.
  _Why:_ `cargo clippy --all-targets` is now warning-free, so a future real
  finding will not be lost in existing noise.


- **Phase 1.1 — configuration data model** (`src/config/mod.rs`): `Config`,
  `ServerConfig`, `RouteConfig`, `Redirect`, `Method`, plus `Config::load` tying
  parse, path resolution and validation together, and `ConfigError` for the
  three ways loading can fail (unreadable / syntax / semantic).
  _Why:_ the struct fields are a one-to-one transcription of the subject's
  "Configuration File" bullet list. Anything missing from the model is a feature
  the config cannot express, and the auditor tests each bullet separately
  (`[A-9]`..`[A-16]`).

- **Phase 1.2 — hand-written YAML-subset parser** (`src/config/parser.rs`):
  nested mappings, block sequences, sequences of mappings, `#` comments, quoted
  scalars with escapes, and `K`/`M`/`G` size suffixes. Two passes — text to a
  generic `Yaml` tree, then tree to the typed `Config`.
  _Why:_ keeps the dependency set at `mio` + `libc`, and every error carries the
  line it occurred on. The two-pass split keeps syntax errors ("expected
  `key: value`") distinguishable from schema errors ("unknown key
  `serevr_names`"), which is the difference between a diagnosable config mistake
  and a mysterious one.

- **Unsupported syntax is rejected by name, not mis-parsed.** Tabs, flow-style
  collections (`[a, b]`), unknown keys, unknown methods and duplicate keys are
  all errors that name the supported alternative.
  _Why:_ a config that is silently read differently from how it was written is
  the worst possible outcome — the server looks broken while the file looks
  fine. `ports: [8080]` now says "write a block list instead" rather than
  failing later with "`[8080]` is not a valid port number".

- **Relative paths resolve against the config file's directory,** not the
  process working directory.
  _Why:_ `localserver /etc/localserver/config.yaml` must work from any working
  directory; resolving against the cwd would make the same config behave
  differently depending on where it was started.

- **Phase 1.3 — startup validation** (`src/config/validate.rs`) with two
  severities: `Fatal` (listener conflicts — refuse to start) and `Skip` (one
  broken server block — drop it, keep the others). Per-server checks run first;
  conflict detection then runs over the survivors only.
  _Why:_ this is the whole of `[A-30]` and `[A-31]`, which pull in opposite
  directions. A duplicate port is unresolvable — the server cannot guess which
  block wins — so it must fail loudly at startup rather than at `bind()` time in
  production. A missing document root affects only its own server, so it must
  not take down the others. The ordering matters for the same reason: a server
  already dropped will never bind a socket, so it cannot conflict with anything,
  and checking it would turn one broken block into a fatal error.

- **Same `host:port` with distinct `server_names` is explicitly allowed,** and
  covered by its own test.
  _Why:_ that is ordinary virtual hosting (`[A-11]`), not a conflict. Without a
  test pinning it, an over-eager duplicate-port check would look correct while
  breaking the vhost requirement.

- **Phase 1.4 — 41 unit tests** across parser and validation: node shapes,
  comment and quote handling, size and boolean spellings, every error path with
  its expected line number, all four fatal conflict shapes, ten skip conditions,
  and the Skip/Fatal boundary.
  _Why:_ required by `[A-35]` and `[A-37]`. Two are regression tests against the
  shipped `config.yaml` itself, so a syntax the parser cannot handle cannot be
  introduced into the real file unnoticed.

- **`main.rs` loads and prints the configuration** — listeners, virtual hosts,
  body limits and routes — and exits 2 on an unusable config.
  _Why:_ makes a config change confirmable without sending a request, which is
  most of what the audit's configuration section asks for. The distinct exit code
  lets a test script tell "bad config" from any other failure.


- **Phase 0.1 — Cargo project and module tree.** `cargo init --name localserver`
  (edition 2021) with `mio 1.2` (`os-poll`, `net`, `os-ext`) and `libc 0.2` as the only
  dependencies. Created `src/config/{mod,parser,validate}.rs`, `src/server.rs`,
  `src/connection.rs`, `src/http/{mod,request,chunked,multipart,response}.rs`,
  `src/router.rs`, `src/handlers/{mod,session_page}.rs`, `src/cgi.rs`, `src/error.rs`,
  `src/utils/{mod,session,cookie,mime,path}.rs` (`multipart` added in Phase 3.3, `session_page` in Phase 6.2; `os-ext` provides `SourceFd` for CGI pipes).
  _Why:_ the audit is conducted by reading the source aloud ("show me where you
  check the return value of `write`"). A flat `main.rs` turns every such question
  into an archaeology exercise; one file per concern makes each answer a jump to a
  known location. Modularisation is also an explicit tip in the subject.

- **Doc comment on every module** naming its responsibility, the `TASKS.md` task
  that will fill it in, and the audit questions it answers.
  _Why:_ the design decisions that matter (one read per event, `WouldBlock` is not
  an error, CGI pipes on the same `epoll`) are invisible in an empty file and easy
  to forget by the time the code is written. Recording them at creation time keeps
  the implementation honest to the plan and gives the audit walkthrough its
  anchors.

- **`mio` + `libc` as the entire dependency set.**
  _Why:_ the subject forbids frameworks and async runtimes. `mio` is a thin
  readiness wrapper over `epoll` with no executor, so it stays on the right side
  of that line; `libc` is needed only for the syscalls `mio` does not expose —
  `fcntl(O_NONBLOCK)` on CGI pipes, `waitpid`, and ignoring `SIGPIPE`.

- **Phase 0.2 — `config.yaml`**, with three server blocks covering every
  configurable feature: two ports on one server (8080/8081), `localhost` and
  `test.com` sharing port 8080 as virtual hosts, a third server on 8082, custom
  error pages, per-route accepted methods, a 301 redirect, autoindex, CGI by file
  extension, an upload directory, and body limits of 1M and 1K.
  _Why:_ "avoid hardcoding; use the config file". The auditor edits this file and
  re-runs the server (`[A-9]`..`[A-16]`), so every feature needs a working example
  to edit rather than one to invent on the spot. The 1K limit on port 8082 makes
  `[A-13]` (413 on an oversized body) a one-line `curl`.

- **A `/readonly` route pointing at the same directory as `/uploads`,** but
  without `DELETE` in its accepted methods.
  _Why:_ `[A-16]` asks for a DELETE attempt *with and without* permission. Two
  routes over one directory make that a two-command demonstration instead of a
  configuration edit performed live during the audit.

- **`www/`** — `index.html` linking every testable path, `static/style.css`,
  `static/logo.png` (generated 48×48 PNG), `empty.html`, `listing/` (deliberately
  without an index file, with a nested subdirectory), and `site2/` for the
  `test.com` virtual host.
  _Why:_ several audit questions need real files to point at: an image and a
  stylesheet prove MIME detection and keep-alive reuse in the browser's network
  panel (`[A-24]`); `listing/` cannot be autoindexed if it contains an
  `index.html`, hence the deliberate omission (`[A-26]`); the nested subdirectory
  catches the trailing-slash bug that breaks relative links one level down;
  `empty.html` is the target `siege` hits in `[A-32]`.

- **`error_pages/`** — 400, 403, 404, 405, 408, 413, 500, 501.
  _Why:_ the six the subject mandates, plus 408 (needed once request timeouts land
  in `TASKS.md` 2.6) and 501 (unknown method). Written now because `config.yaml`
  references them by path and startup validation will reject a config pointing at
  a missing file.

- **`cgi-bin/` and `uploads/`,** each with a `.gitkeep`.
  _Why:_ git does not track empty directories, but config validation will fail at
  startup if a route root does not exist — so a fresh clone has to contain them.

- **`TASKS.md`** — 11 phases, 40 tasks, each mapped to the audit questions.
  _Why:_ the audit is a fixed list of questions; ordering the work by which
  questions it answers stops effort going into parts nobody checks, and makes the
  reasoning available at audit time rather than reconstructed.

- **`.gitignore`** — build output, runtime uploads and logs, stress-test and
  profiling artifacts.
  _Why:_ `siege`, `valgrind` and `perf` all drop files into the working directory
  during Phase 9, and uploaded files are user data that must never enter git.
  `Cargo.lock` is deliberately *not* ignored: this crate builds a binary, and the
  audit should compile the same dependency versions that were tested.

- **`README.md`** — build and run instructions, design table, module map, and a
  configuration reference.
  _Why:_ the audit opens with "take the necessary time to understand the project".
  A README that names the single-thread/single-`epoll` design and maps the modules
  answers the orientation questions before they are asked.

- **`CHANGELOG.md`** — this file.
  _Why:_ it doubles as the audit trail: each entry ties a change to the audit
  question that motivated it.

### Changed

- **`release` profile pinned to `panic = "unwind"`.**
  _Why:_ `panic = "abort"` is a common release setting and would silently disable
  the per-connection `catch_unwind` planned in `TASKS.md` 2.7 — the last-resort
  net that turns a logic bug into a 500 and a dropped client instead of a dead
  process. "Never crash" is a hard requirement, so the profile has to preserve
  unwinding.

- **Removed the speculative `www/uploads/` directory and its ignore rules.**
  _Why:_ it was created before `config.yaml` existed. The config now settles
  uploads at the repo-root `uploads/`, and a second, unreferenced upload directory
  would be a misleading thing for an auditor to find.

- **Changelog compare link repointed** to
  `platform.zone01.gr/git/sntentop/rust-localserver`.
  _Why:_ the placeholder pointed at a GitHub URL inferred from the local git user
  name; the actual remote is the Zone01 Gitea instance.

### Notes

- Every mandatory feature in the subject is now implemented: the event loop,
  configuration, HTTP/1.1 parsing, routing, static files, uploads, deletion,
  directory listings, redirects, error pages, virtual hosts, cookies, sessions
  and CGI. What remains in `TASKS.md` is Phase 8 (the metrics bonus), Phase 9
  (the integration test suite and the hardening pass) and Phase 10 (audit
  rehearsal).
- **CGI throughput is bounded by the interpreter, not the server.** Measured on
  this machine: `hello.py` takes 187 ms run directly and 190 ms through the
  server — about 3 ms of overhead — because Python's startup dominates. The same
  request in Perl is 42 ms. Under `siege` the CGI endpoint sustains 100%
  availability at ~5 trans/sec with 25 concurrent users on 4 cores, which is 25
  interpreters starting at once rather than anything the loop is doing.
- The `/session` paths are built in rather than configured, like the metrics
  endpoint planned for Phase 8: they report on the server itself, so there is no
  document root they could sensibly be mapped to. The prefix is a single
  constant.
- **Gap worth closing:** the edge-triggered deadlock fixed this phase is
  invisible to unit tests, which never touch a socket. Until the integration
  suite in Phase 9.1 exists there is no automated guard against it returning; it
  was caught by hand with a 700 KB POST.
- `cargo build`, `cargo build --release` and `cargo clippy --all-targets` are
  clean; `cargo test` runs 117 passing tests.
- **siege, 15 s, default 25 users**: `/empty.html` 57,671 transactions at 3809
  trans/sec and `/cgi-bin/hello.py` 79 transactions at 5 trans/sec, **100.00%
  availability and 0 failed on both**. Afterwards: descriptors at the
  7-descriptor baseline, RSS unchanged, no children, no zombies, no
  `CLOSE_WAIT`. File descriptors returned to the 7-descriptor baseline and no
  `CLOSE_WAIT` sockets remained. The 99.5% target in `[A-32]` is met, though it
  must be re-measured once the filesystem is in the path.
- Not yet done: `Method::Put`/`Options` parse but no handler implements them.
  The multipart parser and MIME table are written and tested but not yet called
  (Phase 5), and are marked `#[allow(dead_code)]` with the task that consumes
  them.
- Toolchain: `rustc` / `cargo` 1.97.1. Resolved dependencies: `mio 1.2.3`,
  `libc 0.2.189` (`log 0.4.34` arrives transitively via `mio`).
- `siege` is not installed on the development machine; required before Phase 9
  (`[A-32]`).

<!--
Template for the first release:

## [0.1.0] - YYYY-MM-DD

### Added
- Single-threaded, single-`epoll` event loop over `mio` (`[A-2]`..`[A-8]`).
- HTTP/1.1 request parser: request line, headers, `Content-Length` and
  chunked bodies (`[A-36]`).
- Configuration parser and startup validation, incl. duplicate-port
  detection (`[A-30]`, `[A-31]`, `[A-37]`).
- Routing: virtual hosts, longest-prefix routes, per-route methods,
  redirects, autoindex, CGI (`[A-14]`..`[A-16]`, `[A-38]`).
- `GET` / `POST` / `DELETE`, file uploads, custom error pages
  (`[A-12]`, `[A-17]`..`[A-21]`).
- Cookie and in-memory session support (`[A-22]`).
- Non-blocking CGI execution with pipes on the same `epoll` (`[A-28]`).
-->

[Unreleased]: https://platform.zone01.gr/git/sntentop/rust-localserver/compare/v0.1.0...HEAD
