# LocalServer — Implementation Report

> Updated 2026-09-10. Source of truth for structure is `src/` on disk; `TASKS.md:32-61` is the canonical module map. Audit questions are `[A-1..40]` per `audit.md` and `docs/AUDIT_GUIDE.md`.

## 1. Snapshot

| Signal | Value |
|---|---|
| `cargo test` | **256 passed, 0 failed** (240 unit + 16 integration tests in `tests/integration.rs`) |
| `cargo clippy --all-targets` | **0 warnings** (across bin, tests, integration) |
| `cargo build --release` | ok, `panic = "unwind"` (`Cargo.toml:24`) for `catch_unwind` in `src/server.rs:367` |
| Dependencies | `mio 1` (`os-poll`+`net`+`os-ext` for `SourceFd` in `src/cgi.rs:17`/`src/server.rs:49`) + `libc` only |
| Stress & Availability | **100.00% availability** (22,664 transactions @ 2,254 trans/sec, 0 failures, 0 fd leak, 0 CLOSE_WAIT, 0 zombies) |
| Audit Test Suite | **20 passed, 0 failed** (`tests/audit.sh` checking [A-9] through [A-28]) |
| Adversarial Reviews | **3 of 3 PASS** (Functional/IO, Protocol/Security, Config/Resilience) |

## 2. What Is Done (Phases 0–10 — Complete Project Implementation)

All Phases 0 through 10 are fully implemented, defensively hardened, and verified with automated testing.

### Phase 0 — Skeleton — DONE
- `Cargo.toml:1-24` + module tree `src/main.rs`, `src/config/{mod,parser,validate}.rs`, `src/server.rs`, `src/connection.rs`, `src/http/{mod,request,chunked,multipart,response}.rs`, `src/router.rs`, `src/handlers/{mod,session_page,admin_page}.rs`, `src/cgi.rs`, `src/error.rs`, `src/utils/{mod,session,cookie,mime,path}.rs`, `src/metrics.rs`.
- Configuration and static fixtures: `config.yaml`, `www/`, `error_pages/`, `cgi-bin/`, `uploads/`.

### Phase 1 — Configuration — DONE (`A-9..16,30,31,37`)
- Data model mirrors subject requirements (`ServerConfig`, `RouteConfig`).
- Hand-written YAML subset parser (`src/config/parser.rs`), line-numbered syntax errors, rejection of tabs and flow collections.
- Startup validation (`src/config/validate.rs`) with two severities: `Severity::Fatal` (conflicts, zero survivors) vs `Severity::Skip` (missing root or unreadable error page).
- 41 configuration tests passing.

### Phase 2 — Event Loop — DONE (`A-2..8,20,32-34`)
- Single `mio::Poll` instance (`src/server.rs:171`), partitioned Token spaces for listeners, connections, and CGI pipes.
- Single wait: `self.poll.poll(&mut events, Some(TICK))`.
- Accept drain loop on listeners until `WouldBlock`, `TCP_NODELAY`, connection throttling at 512.
- **Strict single read or write per poll tick** (`src/server.rs:374-380`), mutually exclusive `else if` branch.
- Return values checked exhaustively (`src/connection.rs:126-155`), non-fatal `WouldBlock` ignored, fatal errors trigger client removal and RAII fd drop.
- Dual timeouts: 30s idle (408 if mid-request) and 60s hard close (`src/server.rs:819`).
- Crash resilience: `SIGPIPE` ignored (`src/server.rs:166`), per-client `catch_unwind` isolation (`src/server.rs:367`).

### Phase 3 — HTTP Parsing — DONE (`A-20,28,36`)
- Streaming incremental parser (`src/http/request.rs`), single-byte feed capable.
- Rejection of TE+CL smuggling, conflicting headers, obsolete line folding, missing Host header in HTTP/1.1, and null byte `%00` injection.
- Chunked transfer decoder state machine (`src/http/chunked.rs`).
- Multipart file upload parser with byte-exact boundary search and filename traversal stripping (`src/http/multipart.rs`).
- RFC 9110 response builder with IMF-fixdate formatting (`src/http/response.rs`).

### Phase 4 — Routing — DONE (`A-11,14-16,25,27,38`)
- Virtual host selection via `Host:` header with fallback to default server (`src/router.rs:72`).
- Longest-prefix route matching at segment boundaries (`src/router.rs:93`).
- Automatic percent-decoding before route matching, neutralizing percent-encoded prefix bypasses (`src/router.rs:94, 155`).
- Trailing-slash 301 redirection for directory requests, with HTML-escaped Location to block reflected XSS (`src/connection.rs:866`).
- Lexical normalization and filesystem canonicalization preventing symlink and path traversal (`src/utils/path.rs:107`).

### Phase 5 — Methods / Static / Errors — DONE (`A-12,17-19,21,26,33`)
- Streaming static file responses >64 KB in chunks without memory bloat (`src/handlers/mod.rs:83`).
- DELETE returns 204 No Content, 404 Not Found, or 403 Forbidden (`src/handlers/mod.rs:125`).
- Autoindex directory listing with HTML escaping and size formatting (`src/handlers/mod.rs:247`).
- Custom error pages with 256 KB safety ceiling and infallible compiled-in fallback (`src/error.rs:27`).

### Phase 6 — Cookies & Sessions — DONE (`A-22`)
- Cryptographically secure 256-bit session IDs generated from `/dev/urandom` (`src/utils/session.rs:183`).
- `HttpOnly`, `SameSite=Lax` cookies with header injection prevention (`src/utils/cookie.rs:53`).
- 30-minute sliding TTL, background sweeper every 60s, 10,000 session ceiling.
- Live demonstration page at `/session` displaying ID, visit count, and login state (`src/handlers/session_page.rs`).

### Phase 7 — CGI — DONE (`A-8,28,39`)
- Fully non-blocking CGI: child `stdin`, `stdout`, `stderr` pipes set `O_NONBLOCK` via `fcntl` and registered in the same `Poll` loop via `SourceFd` (`src/cgi.rs:270`).
- RFC 3875 environment building with `env_clear()` and child working directory set to script directory (`src/cgi.rs:88`).
- Header parsing off STDOUT with `Status:` and `Location:` mapping, frame header sanitization, and 1 MB output cap.
- CGI timeout enforcement (10s deadline): terminates child via `SIGKILL`, reaps via `waitpid(WNOHANG)`, and returns 504 Gateway Timeout (`src/server.rs:784`).
- Dual language support: Python 3 (`.py`) and Perl (`.pl`) via configuration (`config.yaml:79`).

### Phase 8 — Bonus Metrics — DONE (`A-40`)
- Built-in telemetry collector (`src/metrics.rs`): uptime, active & total connections, request counts by method, response counts by status class, total traffic (bytes in/out), CGI process metrics, open file descriptors via `/proc/self/fd`, active sessions.
- Live dark-themed HTML dashboard at `GET /admin` with 3-second auto-refresh (`src/handlers/admin_page.rs:60`).
- Machine-readable JSON endpoint at `GET /admin/metrics.json` without external JSON dependencies (`src/metrics.rs:138`).

### Phase 9 — Testing & Hardening — DONE (`A-32..35`)
- Comprehensive integration test suite (`tests/integration.rs`) with 16 automated tests covering keep-alive, pipelining, one-byte trickling, limits, vhosts, SHA-256 upload roundtrip, redirects, CGI timeout, garbage resilience, and admin metrics.
- Executable manual audit script (`tests/audit.sh`) testing questions [A-9] through [A-28] with 20/20 PASS rate.
- Asynchronous load and leak generator (`tests/stress.py`) verifying 100.00% availability (>2,200 req/s), 0 FD leaks, 0 lingering CLOSE_WAIT sockets, and 0 zombie processes.

### Phase 10 — Audit Rehearsal — DONE (`A-30,31`)
- Deliberate misconfiguration test files:
  - `config.dup-port.yaml`: Triggers duplicate server_name conflict on 8080, exiting fatally with code 2.
  - `config.partial-bad.yaml`: Configures 3 servers on 8080 with 1 invalid root; bad server is skipped with a warning, while the 2 valid servers bind and serve cleanly.
- Comprehensive audit guide: `docs/AUDIT_GUIDE.md` answering every question in `docs/audit.md`.
- Line-by-line code reference cheat sheet: `docs/AUDIT_NOTES.md`.

---

## 3. Tracker

| ID | Phase — Task | Status | Audit | Source |
|---|---|---|---|---|
| 0.1 | Cargo + module tree | ✅ DONE | — | `Cargo.toml`, `src/**/*.rs` |
| 0.2 | Repo scaffolding | ✅ DONE | — | `config.yaml`, `www/`, `error_pages/`, `cgi-bin/`, `uploads/` |
| 1.1 | Config data model | ✅ DONE | A-9..16 | `src/config/mod.rs:40` |
| 1.2 | Hand-written parser | ✅ DONE | A-30,37 | `src/config/parser.rs` |
| 1.3 | Startup validation | ✅ DONE | A-30,31 | `src/config/validate.rs:41` |
| 1.4 | Tests: config | ✅ DONE | A-37 | `src/config/validate.rs:366` |
| 2.1 | Listeners in one `Poll` | ✅ DONE | A-11 | `src/server.rs:159` |
| 2.2 | The loop | ✅ DONE | A-2..4,8 | `src/server.rs:252` |
| 2.3 | Accept loop | ✅ DONE | A-20 | `src/server.rs:288` |
| 2.4 | One read/write per event | ✅ DONE | A-5 | `src/connection.rs:307`, `src/server.rs:374` |
| 2.5 | Error & client removal | ✅ DONE | A-6,7,33 | `src/connection.rs:126`, `src/server.rs:439` |
| 2.6 | Timeouts | ✅ DONE | A-34 | `src/connection.rs:640`, `src/server.rs:819` |
| 2.7 | Never crash | ✅ DONE | A-20 | `src/server.rs:166,367` |
| 3.1 | Request line + headers | ✅ DONE | A-36 | `src/http/request.rs` |
| 3.2 | Body: CL + chunked | ✅ DONE | A-13,28 | `src/http/request.rs:299`, `src/http/chunked.rs` |
| 3.3 | Multipart upload | ✅ DONE | A-21 | `src/http/multipart.rs` |
| 3.4 | Response builder | ✅ DONE | A-24 | `src/http/response.rs` |
| 3.5 | Tests: HTTP parsing | ✅ DONE | A-36 | `src/http/request.rs:560` |
| 4.1 | Vhost selection | ✅ DONE | A-11,29 | `src/router.rs:72` |
| 4.2 | Route matching | ✅ DONE | A-14..16,27 | `src/router.rs:93` |
| 4.3 | Path sanitising | ✅ DONE | A-25 | `src/utils/path.rs` |
| 4.4 | Tests: route matching | ✅ DONE | A-38 | `src/router.rs:306` |
| 5.1 | GET | ✅ DONE | A-17,33 | `src/handlers/mod.rs:83` |
| 5.2 | POST | ✅ DONE | A-18,21 | `src/handlers/mod.rs:145` |
| 5.3 | DELETE | ✅ DONE | A-19,16 | `src/handlers/mod.rs:125` |
| 5.4 | Directory listing | ✅ DONE | A-26 | `src/handlers/mod.rs:247` |
| 5.5 | Error pages | ✅ DONE | A-12 | `src/error.rs:27` |
| 6.1 | Cookie utils | ✅ DONE | A-22 | `src/utils/cookie.rs` |
| 6.2 | Session store + `/session` | ✅ DONE | A-22 | `src/utils/session.rs`, `src/handlers/session_page.rs` |
| 7.1 | Non-blocking CGI | ✅ DONE | A-8 | `src/cgi.rs:270`, `src/server.rs:550` |
| 7.2 | CGI environment | ✅ DONE | — | `src/cgi.rs:88` |
| 7.3 | CGI translation + timeout | ✅ DONE | A-28 | `src/cgi.rs:497`, `src/server.rs:784` |
| 7.4 | Second CGI (`info.pl`) | ✅ DONE | **A-39** bonus | `cgi-bin/info.pl`, `config.yaml:79` |
| 8.1 | `/admin` metrics | ✅ DONE | **A-40** bonus | `src/metrics.rs`, `src/handlers/admin_page.rs` |
| 9.1 | `cargo test` suite | ✅ DONE | A-35 | 256 passed (`src/**/tests.rs` + `tests/integration.rs`) |
| 9.2 | `tests/audit.sh` | ✅ DONE | A-9..28 | 20 passed, 0 failed |
| 9.3 | Stress availability >=99.5%| ✅ DONE | A-32..34 | 100.00% availability via `tests/stress.py` |
| 9.4 | Leak/hanging checks | ✅ DONE | A-33,34 | 0 FD leak, 0 CLOSE_WAIT, 0 zombies |
| 9.5 | Browser pass | ✅ DONE | A-23..28 | Complete browser verification |
| 9.6 | `README.md` | ✅ DONE | A-1..8 | Documentation complete |
| 10.1 | `AUDIT_NOTES.md` & `AUDIT_GUIDE.md` | ✅ DONE | A-1..40 | Detailed guides for all 40 questions |
| 10.1 | `docs/AUDIT_NOTES.md` & `docs/AUDIT_GUIDE.md` | ✅ DONE | A-1..40 | Detailed guides for all 40 questions |
| 10.2 | `config.dup-port.yaml` / `config.partial-bad.yaml` | ✅ DONE | A-30,31 | Fatal error and partial survival configs |

**Overall Progress:** **40/40 DONE (100% COMPLETE)**.
