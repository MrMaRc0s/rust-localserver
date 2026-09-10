# LocalServer — Implementation Report

> Generated 2026-09-10. Source of truth for structure is `src/` on disk; `TASKS.md:32-61` is the canonical module map. Audit questions are `[A-n]` per `TASKS.md:569-612` / `audit.md`.

## 1. Snapshot

| Signal | Value |
|---|---|
| `cargo test` | **221 passed, 0 failed** (`src/**/*.rs` unit tests) |
| `cargo clippy --all-targets` | **0 warnings** |
| `cargo build --release` | ok, `panic = "unwind"` (`Cargo.toml:24`) for `catch_unwind` in `src/server.rs:367` |
| Dependencies | `mio 1` (`os-poll`+`net`+`os-ext` for `SourceFd` in `src/cgi.rs:17`/`src/server.rs:49`) + `libc` only |
| `siege` | **not installed** (`TASKS.md:9`) — historical 57,671 trans @3809/s 100% in `README.md:28` stale |
| `tests/` dir | **absent** — integration suite & `tests/audit.sh` missing |

## 2. What Is Done (Phases 0–7 — every mandatory feature)

**All of Phase 0–7 is implemented and defensible at the audit.** Each row can be shown live from source.

### Phase 0 — Skeleton — DONE

- `Cargo.toml:1-24` + module tree `src/main.rs`, `src/config/{mod,parser,validate}.rs`, `src/server.rs`, `src/connection.rs`, `src/http/{mod,request,chunked,multipart,response}.rs`, `src/router.rs`, `src/handlers/{mod,session_page}.rs`, `src/cgi.rs`, `src/error.rs`, `src/utils/{mod,session,cookie,mime,path}.rs`
- `config.yaml` 3 servers (8080/8081 + vhost `test.com` on 8080 + 8082 tiny 1K), `www/{index.html,empty.html,static/style.css,logo.png,listing/,site2/}`, `error_pages/8`, `cgi-bin/{hello.py,form.py,slow.py,info.pl,data.txt}`, `uploads/.gitkeep`

### Phase 1 — Configuration — DONE (`A-9..16,30,31,37`)

- Data model `src/config/mod.rs:40` mirrors subject bullets.
- Parser `src/config/parser.rs` two-pass YAML subset, line-numbered errors, rejects tabs/flow.
- Validation `src/config/validate.rs:41` two severities: `Fatal` (duplicate `host:port`+`server_name`) vs `Skip` (bad root/page/body), survivors-only conflict check. 41 tests `parser.rs`+`validate.rs`.

### Phase 2 — Event Loop — DONE (`A-2..8,20,32-34`)

- `src/server.rs:159` one `Poll`/`Events(1024)`, one `TcpListener` per unique `host:port`, tokens `[0,1<<20)` listeners / `[1<<20,2<<20)` conns / `≥2<<20` CGI.
- `server.rs:252` loop `poll(Some(500ms))` → dispatch → `sweep_timeouts`+`sweep_cgi`+`sweep_sessions`.
- `server.rs:288` accept-drain loop until `WouldBlock`, `TCP_NODELAY`, throttling at 512 conns.
- `src/connection.rs:307,327` **exactly one `read()`/`write()` per event**, `desired_interest` never both, `rearm`+`server.rs:412` `EPOLL_CTL_MOD` fixes edge-triggered deadlock (`CHANGELOG.md:353`).
- `connection.rs:126` `classify_read/write → Progress/PeerClosed/WouldBlock/Failed`; `WouldBlock` not an error.
- Timeouts `connection.rs:90` `IDLE 30s`→408 if mid-request else silent / `REQUEST 60s` hard close.
- Never-crash `server.rs:37` `deny(unwrap)`, `SIGPIPE SIG_IGN:165`, `catch_unwind:367`.

### Phase 3 — HTTP Parsing — DONE (`A-20,28,36`)

- Incremental parser `src/http/request.rs` `NeedMore/Complete/Failed(status)`, rejects `CL+TE`/`Host`/`%00`/`OBSFOLD` etc., `src/http/chunked.rs` state machine, `src/http/multipart.rs:141` `sanitize_filename`, `src/http/response.rs` `Date` via Hinnant + `HEAD`.
- 117+ tests including one-byte-at-a-time pipelined.

### Phase 4 — Routing — DONE (`A-11,14-16,25,27,38`)

- `src/router.rs:72` vhost `Host` strip `:port` lowercasing → `is_default` → first.
- `router.rs:93` longest-prefix segment-boundary, order redirect→405(`Allow`)→sanitise→CGI→dir(index/autoindex/403)→file.
- `src/utils/path.rs:37` `percent_decode`→`normalize_segments`→`canonicalize` symlink guard.

### Phase 5 — Methods / Static / Errors — DONE (`A-12,17-19,21,26,33`)

- `src/handlers/mod.rs:83` streaming >64 KB via `FileBody` one chunk/write event, inline ≤64 KB; `DELETE:130` 204/404/403; `listing:247` HTML-escaped href vs text, dirs-first; `src/error.rs:31` `page→builtin` 256 KB cap `A-12`.

### Phase 6 — Cookies & Sessions — DONE (`A-22`)

- `src/utils/cookie.rs:40` `HttpOnly+SameSite=Lax` `Secure` off, injection reject.
- `src/utils/session.rs` 32B `/dev/urandom` hex `Rc<RefCell>`, 30 min sliding TTL swept `server.rs:797` 1/min, 10k cap, `/session` demo `src/handlers/session_page.rs` + `src/connection.rs:672`.

### Phase 7 — CGI — DONE (`A-8,28,39`)

- `src/cgi.rs:250` `Command` piped `O_NONBLOCK` `SourceFd` same `Poll`, one syscall per CGI event `server.rs:639` re-arm, `waitpid(WNOHANG)`+`Drop` kill.
- Env `cgi.rs:94` `GATEWAY_INTERFACE,SCRIPT_FILENAME,PATH_INFO,PATH_TRANSLATED,…` `env_clear`, `current_dir` script dir + canonicalised path fix.
- `cgi.rs:497` `Status:`→code, `Location→302`, discards framing, `MAX_CGI_OUTPUT 1M`, `CGI_TIMEOUT 10s`→504 `server.rs:766`. Bonus `cgi-bin/info.pl` `.py/.pl` config-only `config.yaml:79`.

## 3. What Remains (Phases 8–10)

### Phase 8 — Bonus Metrics — NOT STARTED (`A-40`)

| Gap | Required by `TASKS.md:460-470` |
|---|---|
| No counters | uptime, total/active conns, req by method, resp by class, bytes in/out, CGI spawned/failed/timed-out |
| No endpoint | `GET /admin` HTML + `GET /admin/metrics.json` |
| No fd count | `/proc/self/fd` via `connection.rs:285` TODO |
| No wiring | `src/server.rs` metrics struct + request hook + `config.yaml` route is built-in (like `/session`) |

### Phase 9 — Testing & Hardening — PARTIAL

| Task | Status | Detail |
|---|---|---|
| **9.1 `cargo test` suite** | **PARTIAL** | Unit 221 DONE; integration `tests/*.rs` ephemeral port `TcpStream` keep-alive/pipelined/one-byte/timeout→408/413/redirect/autoindex: **NOT DONE** (`tests/` missing). `CHANGELOG.md:759` notes the deadlock guard gap. |
| **9.2 `tests/audit.sh`** | **NOT DONE** | Template `TASKS.md:487-498` 8 `curl` per `A-9..22` + `GARBAGE`→400: missing. |
| **9.3 `siege -b`** | **NOT DONE** | Binary missing, `siege.log` absent; README numbers unverifiable. Need `sudo apt install siege` then re-measure static+CGI 60 s for `≥99.5%` `A-32`. |
| **9.4 Leak/hanging checks** | **PARTIAL** | Code correct (`server.rs:439` `Drop` fd, `cgi.rs:442` reap), but `ls /proc/.../fd`→7 baseline, `ss -tan`, `valgrind`, `ps --ppid` not scripted. |
| **9.5 Browser pass** | **PARTIAL** | Manual `www/index.html` via `http://127.0.0.1:8080/` asserted, no checklist artifact. |
| **9.6 `README.md`** | **DONE** | Build/run, config table, design table, module map, layout current after 2026-09-10 fixes. |

### Phase 10 — Audit Rehearsal — NOT DONE

| Task | Status |
|---|---|
| **10.1 `AUDIT_NOTES.md`** `file:line` pointers for `A-1..8` (`server.rs` single `poll`, `connection.rs` `read`/`write`, `classify_*`, `deregister`, `cgi.rs` pipes) | Missing |
| **10.2 Misconfigs** `config.dup-port.yaml` (fatal) + `config.partial-bad.yaml` (2 of 3 survive) for `A-30,31` | Missing (validation supports them) |

## 4. Plan for the Remainder (in `TASKS.md` order — do not shuffle)

> Tactical note from `TASKS.md:618-624`: run `siege` as soon as Phase 2 works; keep 9 continuous, not at the end. Phase 8 + 7.4 bonuses only after mandatory passes end-to-end.

### Next (blocking audit scripts) — 1–2 h

1. **Install `siege`** `sudo apt install siege`.
2. **Create integration suite** `tests/` (Task 9.1):
   - `tests/integration.rs` boots `cargo run -- config.yaml` on ephemeral port, `TcpStream` assertions: GET 200, 404, DELETE 405 `Allow`, POST 2 MB→413 (1 K port 8082), vhost `curl --resolve test.com:8080`, upload `sha256sum` round-trip (binary `logo.png`), redirect `/old-page→301`, pipelined `GET / + GET /empty.html` in one packet, one-byte-at-a-time feed, headers-only→408 after 30 s (or mocked short timeout), autoindex `/listing/`.
   - Guards the edge-triggered deadlock `CHANGELOG.md:759`.
3. **Create `tests/audit.sh`** (Task 9.2) verbatim `TASKS.md:488-498`, `chmod +x`, `cargo test` must include it.
4. **Create misconfigs** (Task 10.2): `config.dup-port.yaml` duplicate `host:127.0.0.1 port:8080 server_names:[dup]` `is_default:true` twice → exit 2; `config.partial-bad.yaml` 3 servers one bad `root: /no/such`+bad `error_pages` → 2 survive with warning.

### Then (evidence for `A-32..34`)

5. **Re-run siege** `siege -b -t60S http://127.0.0.1:8080/empty.html` and `http://127.0.0.1:8080/cgi-bin/hello.py` at 25 ccu; capture `Transactions/Availability` `≥99.5%` into `README.md:28` and `CHANGELOG.md:765`, verify `ls /proc/$(pgrep localserver)/fd | wc -l` returns to 7, `ss -tan | grep 8080` no `CLOSE_WAIT`, `ps --ppid` no zombies.
6. **Browser pass** (Task 9.5) record screenshot/devtools note: 200 `Content-Type`, 404 page, redirect, listing, CGI form POST, upload, session cookie `HttpOnly`.
7. **Write `AUDIT_NOTES.md`** (Task 10.1) one line per `A-1..40` with `file:line` (e.g. `src/server.rs:253` single `poll`, `src/connection.rs:307,327` single syscalls, `src/server.rs:412` re-arm, `src/cgi.rs:281` `SourceFd` same `Poll`).

### Optional bonus

8. **Phase 8 metrics** (Task 8.1) only when 1–7 green: `src/metrics.rs` counters incremented in `server.rs:276` loop + `connection.rs:481` response path, served as built-in route like `/session` (`src/connection.rs:673`, `src/handlers/session_page.rs`) at `GET /admin` + `/admin/metrics.json` (JSON+HTML self-refresh). Must not involve I/O.

## 5. Tracker

*Tracker is also maintained in `TASKS.md:12-26` — this section is the report copy.*

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

**Legend:** ✅ done & tested, 🟨 code correct / evidence missing, ⬜ not started.

**Progress:** 32/40 done, 3 partial, 5 todo (4 mandatory todo + 1 bonus). Mandatory remainder is `9.1(integration)+9.2+9.3+10.1+10.2`.

## 6. File / Folder Structure (actual `src/`)

```
src/main.rs                  entry point
src/config/mod.rs            Config / ServerConfig / RouteConfig
src/config/parser.rs         YAML-subset parser
src/config/validate.rs       startup validation
src/server.rs                Poll / listeners / registry
src/connection.rs            per-client state machine
src/http/mod.rs              re-export hub
src/http/request.rs          incremental parser
src/http/chunked.rs          chunked codec
src/http/multipart.rs        multipart + filename sanitising  (Phase 3.3)
src/http/response.rs         response builder
src/router.rs                Host+path resolution
src/handlers/mod.rs          file/upload/delete/listing/redirect
src/handlers/session_page.rs /session demo               (Phase 6.2)
src/cgi.rs                   non-blocking CGI
src/error.rs                 error pages
src/utils/mod.rs             re-export hub
src/utils/session.rs         session store
src/utils/cookie.rs          Cookie / Set-Cookie
src/utils/mime.rs            MIME
src/utils/path.rs            path sanitising
```

`config.yaml` `www/` `error_pages/` `cgi-bin/` `uploads/` match `TASKS.md:69-72` and `README.md:128-135` after 2026-09-10 struct fixes (`os-ext`, `multipart`, `session_page`).

## 7. Risk / Notes

- The single-`Poll` edge-triggered re-arm is invisible to unit tests; integration tests (Plan §4 step 2) are the only guard against regression `CHANGELOG.md:759`.
- CGI throughput is interpreter-bound (~3 ms overhead `README.md:31`); `siege` must be run on `empty.html` and separately on `cgi-bin/hello.py`.
- `/session` and future `/admin` are built-in routes (no `root`), by design `CHANGELOG.md:755`.

## 8. Commands

```bash
cargo test                         # must stay 221 + new integration
cargo clippy --all-targets         # 0 warnings
cargo run --release -- config.yaml # binds 8080,8081,8082
curl -i http://127.0.0.1:8080/                    # 200
curl --resolve test.com:8080:127.0.0.1 http://test.com:8080/  # vhost A-11
curl -i http://127.0.0.1:8080/nope                # 404
curl -i -X DELETE http://127.0.0.1:8080/readonly/uploads/x  # 405 Allow
printf 'GARBAGE\r\n\r\n' | nc 127.0.0.1 8080      # 400, server alive A-20
./tests/audit.sh                                   # after 9.2
siege -b -t60S http://127.0.0.1:8080/empty.html    # after siege install
```
