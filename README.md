# LocalServer

A HTTP/1.1 web server written in Rust, using **one process, one thread and a
single `epoll` instance** (via [`mio`](https://docs.rs/mio)). No async runtime,
no server framework — `mio` supplies readiness notifications and nothing else.

> Zone01 `rust-localserver` project. The implementation plan lives in
> [`TASKS.md`](TASKS.md); it maps every task to the audit questions in
> [`audit.md`](audit.md).

## Status

Phase 0 complete: project skeleton and scaffolding. The module tree builds and
`cargo test` runs. No request handling yet — see `TASKS.md` for what lands next.

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
| Readiness | Exactly one `read()` or one `write()` syscall per client per readiness event, so no single peer can monopolise the loop. Level-triggered registration re-reports the remainder. |
| Blocking | Nothing blocks. Partial requests and partial responses live in per-connection buffers across loop iterations; CGI stdin/stdout are `O_NONBLOCK` pipes registered in the same `Poll`. |
| Crash safety | No `unwrap`/`expect` on the request path, `SIGPIPE` ignored, per-connection `catch_unwind` as a last resort. A client error drops that client, never the server. |
| Config | Hand-written YAML-subset parser, validated at startup. Dependencies stay at `mio` + `libc`. |

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
| CGI by extension | `cgi: { ".py": /usr/bin/python3 }` |
| Upload destination | `upload_dir: uploads` |

## Layout

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
