//! `Transfer-Encoding: chunked` decoder and encoder.
//!
//! Decoder state machine: `ReadSize -> ReadData -> ReadCrlf -> Trailers -> Done`,
//! fed incrementally like the rest of the parser. The encoder is used for
//! responses of unknown length, chiefly CGI output that declares no
//! `Content-Length`.
//!
//! Both directions matter for audit question [A-28]: a chunked *request* body is
//! de-chunked before it reaches CGI stdin, and a CGI *response* of unknown
//! length goes back out chunked.
//!
//! Filled in by TASKS.md 3.2, 7.3.
