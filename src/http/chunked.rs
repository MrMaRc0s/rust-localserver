//! `Transfer-Encoding: chunked` decoder and encoder.
//!
//! Both directions are needed, and audit question [A-28] asks about both:
//!
//! * **Decoding** — a client (or `curl --data-binary @- -H 'Transfer-Encoding:
//!   chunked'`) may send a body of unknown length. It must be reassembled
//!   before it reaches a handler or a CGI script's stdin.
//! * **Encoding** — a response of unknown length, chiefly CGI output that
//!   declares no `Content-Length`, must go back out chunked.
//!
//! The decoder is a state machine fed incrementally, for the same reason the
//! request parser is: bytes arrive in whatever fragments the network delivers,
//! one `read()` at a time, and a chunk header can be split across two reads.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

/// Longest accepted chunk-size line, including any chunk extensions. A size
/// line longer than this is a malformed or hostile stream, not a real one.
const MAX_SIZE_LINE: usize = 256;

/// Cap on the combined length of trailer fields.
const MAX_TRAILER_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reading the hex chunk-size line.
    Size,
    /// Copying `remaining` bytes of chunk data.
    Data,
    /// Consuming the CRLF that terminates a chunk's data.
    DataCrlf,
    /// Reading trailer fields after the terminating 0-sized chunk.
    Trailers,
    /// The whole body has been decoded.
    Done,
}

/// Outcome of feeding bytes to the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStatus {
    /// More input needed to finish the body.
    NeedMore,
    /// The terminating zero-length chunk and trailers were consumed.
    Done,
    /// Malformed input: answer 400 and close.
    Malformed,
    /// The decoded body exceeded the caller's limit: answer 413.
    TooLarge,
}

pub struct ChunkedDecoder {
    state: State,
    /// Bytes still to copy for the chunk in progress.
    remaining: usize,
    /// Partial size line or trailer block carried between feeds.
    pending: Vec<u8>,
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        ChunkedDecoder::new()
    }
}

impl ChunkedDecoder {
    pub fn new() -> ChunkedDecoder {
        ChunkedDecoder { state: State::Size, remaining: 0, pending: Vec::new() }
    }

    // Read by the CGI response pump in TASKS.md 7.3.
    #[allow(dead_code)]
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Decode as much of `input` as possible, appending body bytes to `out`.
    ///
    /// Returns how many bytes of `input` were consumed together with the
    /// status. A partial chunk header at the end of `input` is retained
    /// internally, so the caller may simply discard the consumed prefix.
    ///
    /// `limit` caps the decoded body: a chunked request cannot announce its
    /// size up front, so the limit has to be enforced *as it arrives* rather
    /// than checked against a `Content-Length` beforehand ([A-13]).
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, limit: usize) -> (usize, ChunkStatus) {
        let mut pos = 0;
        loop {
            match self.state {
                State::Done => return (pos, ChunkStatus::Done),

                State::Size => {
                    // Accumulate until the CRLF that ends the size line.
                    let rest = &input[pos..];
                    let newline = rest.iter().position(|b| *b == b'\n');
                    let (take, complete) = match newline {
                        Some(i) => (i + 1, true),
                        None => (rest.len(), false),
                    };
                    self.pending.extend_from_slice(&rest[..take]);
                    pos += take;
                    if self.pending.len() > MAX_SIZE_LINE {
                        return (pos, ChunkStatus::Malformed);
                    }
                    if !complete {
                        return (pos, ChunkStatus::NeedMore);
                    }
                    let line = std::mem::take(&mut self.pending);
                    match parse_chunk_size(&line) {
                        Some(0) => self.state = State::Trailers,
                        Some(n) => {
                            self.remaining = n;
                            self.state = State::Data;
                        }
                        None => return (pos, ChunkStatus::Malformed),
                    }
                }

                State::Data => {
                    let available = input.len() - pos;
                    if available == 0 {
                        return (pos, ChunkStatus::NeedMore);
                    }
                    let take = self.remaining.min(available);
                    if out.len() + take > limit {
                        return (pos, ChunkStatus::TooLarge);
                    }
                    out.extend_from_slice(&input[pos..pos + take]);
                    pos += take;
                    self.remaining -= take;
                    if self.remaining == 0 {
                        self.state = State::DataCrlf;
                    }
                }

                State::DataCrlf => {
                    // Each chunk's data is followed by CRLF, which is not part
                    // of the body.
                    let rest = &input[pos..];
                    let newline = rest.iter().position(|b| *b == b'\n');
                    let (take, complete) = match newline {
                        Some(i) => (i + 1, true),
                        None => (rest.len(), false),
                    };
                    self.pending.extend_from_slice(&rest[..take]);
                    pos += take;
                    if !complete {
                        if self.pending.len() > 2 {
                            return (pos, ChunkStatus::Malformed);
                        }
                        return (pos, ChunkStatus::NeedMore);
                    }
                    let line = std::mem::take(&mut self.pending);
                    if line.as_slice() != b"\r\n" && line.as_slice() != b"\n" {
                        return (pos, ChunkStatus::Malformed);
                    }
                    self.state = State::Size;
                }

                State::Trailers => {
                    // Zero or more trailer lines, then an empty line.
                    let rest = &input[pos..];
                    let newline = rest.iter().position(|b| *b == b'\n');
                    let (take, complete) = match newline {
                        Some(i) => (i + 1, true),
                        None => (rest.len(), false),
                    };
                    self.pending.extend_from_slice(&rest[..take]);
                    pos += take;
                    if self.pending.len() > MAX_TRAILER_BYTES {
                        return (pos, ChunkStatus::Malformed);
                    }
                    if !complete {
                        return (pos, ChunkStatus::NeedMore);
                    }
                    let line = std::mem::take(&mut self.pending);
                    let trimmed: &[u8] = match line.strip_suffix(b"\r\n") {
                        Some(t) => t,
                        None => line.strip_suffix(b"\n").unwrap_or(&line),
                    };
                    if trimmed.is_empty() {
                        self.state = State::Done;
                        return (pos, ChunkStatus::Done);
                    }
                    // A trailer field: accepted and discarded. Nothing this
                    // server does depends on one.
                }
            }
        }
    }
}

/// Parse a chunk-size line: hex digits, optionally followed by `;extensions`.
fn parse_chunk_size(line: &[u8]) -> Option<usize> {
    let line = match line.strip_suffix(b"\r\n") {
        Some(l) => l,
        None => line.strip_suffix(b"\n")?,
    };
    // Chunk extensions are permitted and ignored.
    let digits = match line.iter().position(|b| *b == b';') {
        Some(i) => &line[..i],
        None => line,
    };
    let text = std::str::from_utf8(digits).ok()?.trim();
    if text.is_empty() || text.len() > 16 {
        return None;
    }
    usize::from_str_radix(text, 16).ok()
}

/// Encode one chunk for a chunked response body.
// Used from TASKS.md 7.3, where CGI output of unknown length is framed.
#[allow(dead_code)]
pub fn encode_chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// The terminating zero-length chunk that ends a chunked response.
#[allow(dead_code)]
pub const CHUNKED_TERMINATOR: &[u8] = b"0\r\n\r\n";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn decode_all(input: &[u8], limit: usize) -> (Vec<u8>, ChunkStatus) {
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let (_, status) = decoder.feed(input, &mut out, limit);
        (out, status)
    }

    #[test]
    fn decodes_a_simple_chunked_body() {
        let (body, status) = decode_all(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", 1024);
        assert_eq!(status, ChunkStatus::Done);
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn decodes_an_empty_body() {
        let (body, status) = decode_all(b"0\r\n\r\n", 1024);
        assert_eq!(status, ChunkStatus::Done);
        assert!(body.is_empty());
    }

    /// The property that matters for a one-read-per-event server: the same
    /// stream split at every possible point must decode identically.
    #[test]
    fn decodes_the_same_body_however_the_stream_is_split() {
        let stream = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        for split in 1..stream.len() {
            let mut decoder = ChunkedDecoder::new();
            let mut out = Vec::new();
            let (consumed, status) = decoder.feed(&stream[..split], &mut out, 1024);
            assert_ne!(status, ChunkStatus::Malformed, "split at {split}");
            let leftover = &stream[consumed..split];
            let mut rest = leftover.to_vec();
            rest.extend_from_slice(&stream[split..]);
            let (_, status) = decoder.feed(&rest, &mut out, 1024);
            assert_eq!(status, ChunkStatus::Done, "split at {split}");
            assert_eq!(out, b"hello world", "split at {split}");
        }
    }

    /// One byte at a time is the worst case, and the strongest evidence that the
    /// decoder never assumes a whole chunk header is present.
    #[test]
    fn decodes_one_byte_at_a_time() {
        let stream = b"4\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n";
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let mut status = ChunkStatus::NeedMore;
        for byte in stream.iter() {
            let (_, s) = decoder.feed(&[*byte], &mut out, 1024);
            status = s;
        }
        assert_eq!(status, ChunkStatus::Done);
        assert_eq!(out, b"abcdefg");
    }

    #[test]
    fn ignores_chunk_extensions() {
        let (body, status) = decode_all(b"5;name=value\r\nhello\r\n0\r\n\r\n", 1024);
        assert_eq!(status, ChunkStatus::Done);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn accepts_and_discards_trailers() {
        let (body, status) = decode_all(b"3\r\nabc\r\n0\r\nX-Checksum: 1234\r\n\r\n", 1024);
        assert_eq!(status, ChunkStatus::Done);
        assert_eq!(body, b"abc");
    }

    #[test]
    fn rejects_a_non_hex_size() {
        let (_, status) = decode_all(b"zz\r\nabc\r\n0\r\n\r\n", 1024);
        assert_eq!(status, ChunkStatus::Malformed);
    }

    #[test]
    fn rejects_a_missing_chunk_terminator() {
        let (_, status) = decode_all(b"3\r\nabcXX4\r\n", 1024);
        assert_eq!(status, ChunkStatus::Malformed);
    }

    /// A chunked body cannot announce its size in advance, so the limit has to
    /// be applied while decoding ([A-13]).
    #[test]
    fn enforces_the_body_limit_while_decoding() {
        let (_, status) = decode_all(b"10\r\n0123456789abcdef\r\n0\r\n\r\n", 8);
        assert_eq!(status, ChunkStatus::TooLarge);
    }

    #[test]
    fn rejects_an_absurdly_long_size_line() {
        let mut input = vec![b'0'; MAX_SIZE_LINE + 10];
        input.extend_from_slice(b"\r\n");
        let (_, status) = decode_all(&input, 1024);
        assert_eq!(status, ChunkStatus::Malformed);
    }

    #[test]
    fn encodes_chunks_and_a_terminator() {
        assert_eq!(encode_chunk(b"hello"), b"5\r\nhello\r\n".to_vec());
        assert_eq!(encode_chunk(b""), b"0\r\n\r\n".to_vec());
        assert_eq!(CHUNKED_TERMINATOR, b"0\r\n\r\n");
    }

    /// What the encoder writes, the decoder must read back.
    #[test]
    fn encoder_and_decoder_round_trip() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_chunk(b"first "));
        wire.extend_from_slice(&encode_chunk(b"second"));
        wire.extend_from_slice(CHUNKED_TERMINATOR);
        let (body, status) = decode_all(&wire, 1024);
        assert_eq!(status, ChunkStatus::Done);
        assert_eq!(body, b"first second");
    }
}
