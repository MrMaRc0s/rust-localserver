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

- No request handling yet: the modules are documented stubs. `cargo build` is
  clean and `cargo test` runs 0 tests, which is the stated Phase 0 exit criterion.
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
