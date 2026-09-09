# Changelog

All notable changes to LocalServer are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries are grouped under `Added`, `Changed`, `Fixed`, `Removed`, `Security`.
Where a change satisfies an audit question, reference it as `[A-n]` using the
numbering in `TASKS.md`.

> **Convention:** this file is updated with **every commit**. The changelog entry
> goes under `## [Unreleased]` and is staged in the *same* commit as the change it
> describes — never as a separate follow-up commit.

## [Unreleased]

### Added
- `TASKS.md` — phased implementation plan (11 phases, 40 tasks) derived from
  `exercise.md`, with each task mapped to the audit questions in `audit.md`.
- `.gitignore` — Rust build output, runtime uploads/logs, stress-test and
  profiling artifacts.
- `CHANGELOG.md` — this file.

### Notes
- No source code yet. Implementation starts at Phase 0 of `TASKS.md`
  (Cargo project + module tree).
- Toolchain in use: `rustc` / `cargo` 1.97.1.
- `siege` is not yet installed on the development machine; required before
  Phase 9 (stress testing, `[A-32]`).

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
