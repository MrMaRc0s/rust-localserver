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

### Added

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
  (edition 2021) with `mio 1.2` (`os-poll`, `net`) and `libc 0.2` as the only
  dependencies. Created `src/config/{mod,parser,validate}.rs`, `src/server.rs`,
  `src/connection.rs`, `src/http/{mod,request,chunked,response}.rs`,
  `src/router.rs`, `src/handlers/mod.rs`, `src/cgi.rs`, `src/error.rs`,
  `src/utils/{mod,session,cookie,mime,path}.rs`.
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

- Routing is complete, but **nothing is read from or written to disk yet**: a
  request that resolves to a file, listing, upload, delete or CGI gets a
  plain-text summary of the decision instead. The handlers that perform the work
  are Phase 5 (and Phase 7 for CGI). Everything the router decides on its own —
  redirects, 405 with `Allow`, 403, 404, 501 — is already final.
- **Gap worth closing:** the edge-triggered deadlock fixed this phase is
  invisible to unit tests, which never touch a socket. Until the integration
  suite in Phase 9.1 exists there is no automated guard against it returning; it
  was caught by hand with a 700 KB POST.
- `cargo build`, `cargo build --release` and `cargo clippy --all-targets` are
  clean; `cargo test` runs 117 passing tests.
- **siege, 20 s, default 25 users, against the routed canned response**: 23,910
  transactions, **100.00% availability**, 0 failed, 1136 trans/sec. File descriptors returned to the 7-descriptor baseline and no
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
