//! URL path to filesystem path resolution, and traversal defence.
//!
//! # The order of operations is the whole point
//!
//! Percent-decode **first**, normalise `.` and `..` **second**, touch the
//! filesystem **last**. Doing it the other way round is how traversal gets
//! through: a normaliser that runs before decoding never sees the `..` hidden
//! in `%2e%2e%2f`, and a `Path::join` on undecoded input silently accepts
//! whatever the client sent.
//!
//! `GET /../../etc/passwd` is the single test an auditor is most likely to try
//! without being asked to.
//!
//! # Why not rely on `canonicalize` alone
//!
//! `canonicalize` fails outright on a path that does not exist, so it cannot be
//! the only defence — a traversal aimed at a non-existent file would skip the
//! check entirely and fall through to a 404 that reveals whether the path
//! exists. Normalising lexically first means the path is already safe before
//! the filesystem is consulted; `canonicalize` is then a *second* check, whose
//! job is symlinks — a link inside the root pointing outside it survives
//! lexical normalisation but not this.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// Percent-decode a URL path segment sequence.
///
/// Returns 400 for a malformed escape and for `%00`: a NUL byte would truncate
/// the path at the first C-string boundary inside a syscall, so
/// `/safe.txt%00../../etc/passwd` could become `/safe.txt` for a validator and
/// something else for the kernel.
pub fn percent_decode(input: &str) -> Result<String, u16> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 2 >= bytes.len() {
            return Err(400);
        }
        let (hi, lo) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]));
        let (hi, lo) = match (hi, lo) {
            (Some(h), Some(l)) => (h, l),
            _ => return Err(400),
        };
        let byte = hi * 16 + lo;
        if byte == 0 {
            return Err(400);
        }
        out.push(byte);
        i += 3;
    }
    // Paths on Linux are bytes, but a non-UTF-8 request path is far more likely
    // to be an encoding attack than a real file name.
    String::from_utf8(out).map_err(|_| 400u16)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Split a decoded path into normalised segments.
///
/// `.` is dropped, `..` pops the previous segment, and a `..` with nothing left
/// to pop is an attempt to escape the root — 403 rather than a silent clamp to
/// the root, because the client asked for something it is not allowed to have.
pub fn normalize_segments(path: &str) -> Result<Vec<String>, u16> {
    let mut stack: Vec<String> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err(403);
                }
            }
            other => {
                // A NUL or a separator surviving decoding is not a file name.
                if other.contains('\0') {
                    return Err(400);
                }
                stack.push(other.to_string());
            }
        }
    }
    Ok(stack)
}

/// Map a URL remainder onto a filesystem path underneath `root`.
///
/// Returns 400 for a malformed or NUL-bearing path, 403 for a traversal
/// attempt (lexical or via symlink).
pub fn resolve_under(root: &Path, url_remainder: &str) -> Result<PathBuf, u16> {
    let decoded = percent_decode(url_remainder)?;
    let segments = normalize_segments(&decoded)?;

    let mut path = root.to_path_buf();
    for segment in &segments {
        // Safe by construction: every segment has been normalised and contains
        // no separator, so this cannot escape `root` lexically.
        path.push(segment);
    }

    verify_under(root, &path)?;
    Ok(path)
}

/// Confirm that a resolved path really lives under `root` once symlinks are
/// followed.
///
/// The path may legitimately not exist yet (a 404, or a file about to be
/// uploaded), so when it does not, the nearest existing ancestor is checked
/// instead — that is the directory a symlink could have redirected.
fn verify_under(root: &Path, path: &Path) -> Result<(), u16> {
    let real_root = match root.canonicalize() {
        Ok(p) => p,
        // The route root vanished after startup validation accepted it.
        Err(_) => return Err(500),
    };

    if let Ok(real) = path.canonicalize() {
        return if real.starts_with(&real_root) { Ok(()) } else { Err(403) };
    }

    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        if let Ok(real) = dir.canonicalize() {
            return if real.starts_with(&real_root) { Ok(()) } else { Err(403) };
        }
        ancestor = dir.parent();
    }
    Err(403)
}

/// Join a URL path with a trailing slash, preserving any query string.
pub fn with_trailing_slash(path: &str, query: &str) -> String {
    let mut out = String::with_capacity(path.len() + query.len() + 2);
    out.push_str(path);
    if !path.ends_with('/') {
        out.push('/');
    }
    if !query.is_empty() {
        out.push('?');
        out.push_str(query);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn decodes_ordinary_escapes() {
        assert_eq!(percent_decode("/a%20b.txt").unwrap(), "/a b.txt");
        assert_eq!(percent_decode("/plain").unwrap(), "/plain");
        assert_eq!(percent_decode("/%C3%A9t%C3%A9").unwrap(), "/été");
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert_eq!(percent_decode("/a%"), Err(400));
        assert_eq!(percent_decode("/a%2"), Err(400));
        assert_eq!(percent_decode("/a%zz"), Err(400));
    }

    /// A NUL byte would truncate the path inside a syscall, making the
    /// validator and the kernel disagree about which file is meant.
    #[test]
    fn rejects_encoded_nul() {
        assert_eq!(percent_decode("/safe.txt%00.png"), Err(400));
        assert_eq!(percent_decode("/%00"), Err(400));
    }

    #[test]
    fn normalises_dot_segments() {
        assert_eq!(normalize_segments("/a/./b").unwrap(), vec!["a", "b"]);
        assert_eq!(normalize_segments("/a/b/../c").unwrap(), vec!["a", "c"]);
        assert_eq!(normalize_segments("//a///b//").unwrap(), vec!["a", "b"]);
        assert_eq!(normalize_segments("/").unwrap(), Vec::<String>::new());
        assert_eq!(normalize_segments("/a/b/..").unwrap(), vec!["a"]);
    }

    #[test]
    fn refuses_to_climb_above_the_root() {
        assert_eq!(normalize_segments("/.."), Err(403));
        assert_eq!(normalize_segments("/../etc/passwd"), Err(403));
        assert_eq!(normalize_segments("/a/../../b"), Err(403));
    }

    /// The attack the ordering exists to stop: `..` hidden inside an escape.
    /// Decoding first is what makes the normaliser see it at all.
    #[test]
    fn catches_traversal_hidden_in_percent_encoding() {
        let decoded = percent_decode("/%2e%2e/%2e%2e/etc/passwd").unwrap();
        assert_eq!(decoded, "/../../etc/passwd");
        assert_eq!(normalize_segments(&decoded), Err(403));

        let decoded = percent_decode("/a/%2E%2E/%2E%2E/b").unwrap();
        assert_eq!(normalize_segments(&decoded), Err(403));
    }

    #[test]
    fn resolves_a_path_under_the_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("www");
        let resolved = resolve_under(&root, "/index.html").unwrap();
        assert_eq!(resolved, root.join("index.html"));
        assert!(resolved.is_file());
    }

    #[test]
    fn resolves_a_nested_path() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("www");
        let resolved = resolve_under(&root, "/listing/nested/deep.txt").unwrap();
        assert!(resolved.is_file(), "{resolved:?}");
    }

    #[test]
    fn a_missing_file_still_resolves_so_the_caller_can_answer_404() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("www");
        let resolved = resolve_under(&root, "/no-such-file.html").unwrap();
        assert!(!resolved.exists());
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn traversal_out_of_the_root_is_forbidden() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("www");
        assert_eq!(resolve_under(&root, "/../Cargo.toml"), Err(403));
        assert_eq!(resolve_under(&root, "/../../etc/passwd"), Err(403));
        assert_eq!(resolve_under(&root, "/%2e%2e/Cargo.toml"), Err(403));
        // Deep enough to escape even after descending first.
        assert_eq!(resolve_under(&root, "/listing/../../Cargo.toml"), Err(403));
    }

    /// Lexical normalisation cannot see a symlink; `canonicalize` is what
    /// catches this one.
    #[test]
    fn a_symlink_pointing_outside_the_root_is_forbidden() {
        let root = std::env::temp_dir().join(format!("localserver-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("secret.txt"), b"secret").unwrap();
        let link = root.join("public").join("escape.txt");
        std::os::unix::fs::symlink(root.join("secret.txt"), &link).unwrap();

        let result = resolve_under(&root.join("public"), "/escape.txt");
        assert_eq!(result, Err(403), "a symlink out of the root must be refused");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builds_trailing_slash_redirect_targets() {
        assert_eq!(with_trailing_slash("/listing", ""), "/listing/");
        assert_eq!(with_trailing_slash("/listing/", ""), "/listing/");
        assert_eq!(with_trailing_slash("/listing", "a=1"), "/listing/?a=1");
    }
}
