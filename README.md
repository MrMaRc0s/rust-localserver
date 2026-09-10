# LocalServer

A HTTP/1.1 web server written in Rust, using **one process, one thread and a
single `epoll` instance** (via [`mio`](https://docs.rs/mio)). No async runtime,
no server framework — `mio` supplies readiness notifications and nothing else.

> Zone01 `rust-localserver` project. The implementation plan lives in
> [`TASKS.md`](TASKS.md); it maps every task to the audit questions in
> [`audit.md`](audit.md).

## Status

Phases 0–7 complete: **every mandatory feature in the subject is implemented.**
GET, HEAD, POST with file uploads, DELETE, directory listings, redirects, custom
error pages, virtual hosts, cookie-backed sessions and CGI — all over a
single-threaded non-blocking event loop. 221 passing tests, clean under
`cargo clippy --all-targets`.

What remains in `TASKS.md` is the metrics bonus (Phase 8), the integration test
suite and hardening pass (Phase 9), and the audit rehearsal (Phase 10).

```bash
cargo run --release -- config.yaml   # binds 8080, 8081, 8082
curl -i http://127.0.0.1:8080/
echo $?                              # 2 if the configuration is unusable
```

Measured with `siege -b -t15S http://127.0.0.1:8080/empty.html`: **100.00%
availability** over 57,671 transactions at 3809 trans/sec, 0 failed. The CGI
endpoint is also 100% available; its throughput is bounded by interpreter
startup (187 ms for python on this machine, of which the server adds ~3 ms). File descriptors return to
baseline and RSS stays flat — a 10 MB download moves it by 128 KB, because
bodies are streamed rather than buffered.

## Build and run

```bash
cargo build --release
./target/release/localserver [CONFIG]   # CONFIG defaults to ./config.yaml
```

Then open <http://127.0.0.1:8080/>.

## Design

| Concern | Approach |
|---|---|
| Concurrency | One process, one thread, one `mio::Poll`. Listeners, client sockets and CGI pipes are all tokens in the same `epoll`. |
| Readiness | Exactly one `read()` or one `write()` syscall per client per readiness event, so no single peer can monopolise the loop. `mio` is edge-triggered, so a socket that may still hold data is **re-armed** with `EPOLL_CTL_MOD` rather than drained in a loop. |
| File bodies | Streamed: a file over 64 KB stays on disk and is pulled one chunk per writable event, so memory never holds more than a chunk. Files under a chunk go out inline with their headers, saving two syscalls and two epoll re-arms on the traffic a static site is mostly made of. |
| Error pages | The page configured for that status, else one compiled into the binary. The fallback is load-bearing: an error page that itself fails would otherwise recurse. |
| CGI | Child processes whose three pipes are `O_NONBLOCK` and registered in the **same `Poll`** as every socket — a slow script never blocks another client. Killed after 10 s (→ 504), reaped with `waitpid(WNOHANG)`, and killed on `Drop` if the client gives up. The interpreter is chosen by file extension in the config, so a second language is a config change. |
| Sessions | 32 bytes from `/dev/urandom`, hex-encoded, in a `HashMap` behind `Rc<RefCell<_>>` — **no `Mutex`**, because one thread means there is nothing to synchronise. Sliding 30-minute expiry, swept from the loop tick once a minute, capped at 10,000. If the CSPRNG is unreadable the server issues no sessions rather than weak ones. |
| Cookies | `HttpOnly` and `SameSite=Lax` by default; `Secure` deliberately off, since a `Secure` cookie would never come back over plain HTTP. Values that could inject a header are refused rather than escaped. |
| Blocking | Nothing blocks. Partial requests and partial responses live in per-connection buffers across loop iterations; CGI stdin/stdout are `O_NONBLOCK` pipes registered in the same `Poll`. |
| Routing | `Host` selects the virtual host; routes match by longest prefix at segment boundaries. Order is fixed: redirect, method (405 with `Allow`), path sanitising, CGI, directory rules, file. A directory with no index and no autoindex is 403, not 404 — it exists, it just may not be enumerated. |
| Traversal | Percent-decode, then normalise `..`, then `canonicalize` to catch symlinks out of the root. Decoding first is what makes `%2e%2e%2f` visible to the normaliser. |
| Parsing | Incremental and restartable: `NeedMore` / `Complete` / `Failed(status)`. "Incomplete" is never treated as an error, which is what lets one-read-per-event coexist with correct parsing. Ambiguous body framing (`Content-Length` **and** `Transfer-Encoding`, or conflicting duplicates) is refused rather than guessed. |
| Crash safety | `unwrap`/`expect` **denied by lint** on the request path, `SIGPIPE` ignored, per-connection `catch_unwind` as a last resort, `EINTR` from `poll()` retried. A client error drops that client, never the server. |
| Timeouts | Two deadlines per connection: 30 s idle and 60 s total. A partial request gets 408; an idle keep-alive connection closes silently, because an expiring connection slot is not an error. |
| Backpressure | At 512 connections the listeners leave the poll, so new clients wait in the kernel backlog instead of spinning the loop on an `accept` that cannot succeed. |
| Config | Hand-written YAML-subset parser, validated at startup. Dependencies stay at `mio` + `libc`. Unsupported syntax is rejected by name rather than mis-parsed. |
| Config errors | Two severities. A **listener conflict** (same `host:port` claimed twice) is unresolvable and refuses startup. A **broken server block** (missing root, unreadable error page) is dropped with a warning while the other servers keep serving. |

### Module map

```
src/main.rs             entry point: args, config load, start server
src/config/mod.rs       Config / ServerConfig / RouteConfig
src/config/parser.rs    hand-written YAML-subset parser
src/config/validate.rs  startup validation (duplicate ports, paths, body limits)
src/server.rs           the event loop: Poll, listeners, connection registry
src/connection.rs       per-client state machine, the only client socket I/O
src/http/request.rs     incremental request parser
src/http/chunked.rs     chunked transfer decoder / encoder
src/http/response.rs    response building and serialization
src/router.rs           Host + path -> route resolution
src/handlers/mod.rs     static files, uploads, delete, autoindex, redirects
src/cgi.rs              CGI spawn and non-blocking pipe pumping
src/error.rs            status codes -> error pages
src/utils/session.rs    in-memory session store
src/utils/cookie.rs     Cookie / Set-Cookie
src/utils/mime.rs       extension -> Content-Type
src/utils/path.rs       path resolution and traversal sanitising
```

## Configuration

`config.yaml` drives everything; nothing about server behaviour is hardcoded.
The shipped file demonstrates each supported feature:

| Feature | Where in `config.yaml` |
|---|---|
| Host and multiple ports | first server: `ports: [8080, 8081]` |
| Multiple servers | three server blocks |
| Virtual hosts on one port | `localhost` and `test.com` both on `127.0.0.1:8080` |
| Default server | `is_default: true` |
| Custom error pages | `error_pages:` map, code -> file |
| Client body limit | `client_max_body_size: 1M` (and `1K` on port 8082) |
| Accepted methods per route | `/uploads` allows DELETE, `/readonly` does not |
| Redirection | `/old-page` -> `301 /index.html` |
| Directory root | `root:` per route |
| Default file for a directory | `index: index.html` |
| Directory listing toggle | `autoindex:` per route |
| CGI by extension | `cgi:` maps `.py` → python3 and `.pl` → perl on one route |
| Upload destination | `upload_dir: uploads` |

## Layout

Session behaviour is visible at <http://127.0.0.1:8080/session>: the id, a visit
counter that climbs on reload, a sliding expiry, and a sign-in form. The cookie
itself is in devtools under Application → Cookies.

CGI lives under <http://127.0.0.1:8080/cgi-bin/>: `hello.py` echoes the
environment it was given (including `PATH_INFO`, `SCRIPT_FILENAME` and a file
read with a *relative* path, which only works because the child runs in the
script's own directory), `form.py` reads a POSTed body, and `info.pl` is the
same route serving a different language — added by one line of config.

`PATH_INFO` is the extra path after the script name, as RFC 3875 defines it:
`/cgi-bin/hello.py/users/7` gives the script `PATH_INFO=/users/7`. The absolute
filesystem paths are in `SCRIPT_FILENAME` and `PATH_TRANSLATED`.

```
config.yaml     server configuration (edit this, not the code)
www/            static site: index, CSS, image, /listing (no index, autoindex)
                and /site2 (the test.com virtual host)
error_pages/    400, 403, 404, 405, 408, 413, 500, 501
cgi-bin/        CGI scripts
uploads/        upload destination (contents git-ignored)
```

## Testing

```bash
cargo test                       # unit + integration suite
./tests/audit.sh                 # curl walkthrough of the audit questions
siege -b 127.0.0.1:8080/empty.html   # availability target: >= 99.5%
```

## License

MIT.
