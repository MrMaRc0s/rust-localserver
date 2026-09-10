//! HTTP/1.1 message types: parsing requests, building responses, and the
//! chunked transfer codec shared by both.
//!
//! Filled in by TASKS.md 3.x.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod chunked;
pub mod multipart;
pub mod request;
pub mod response;
