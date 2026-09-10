//! File extension to `Content-Type` mapping.
//!
//! A static table rather than a crate: the set of types a static site needs is
//! small, and every entry here is one an audit might actually request. A wrong
//! `Content-Type` is immediately visible in the browser's network panel and
//! changes how the page renders, so this is worth getting right ([A-24]).
//!
//! Text types carry `charset=utf-8` because a browser that guesses the encoding
//! renders non-ASCII content wrongly.
//!
//! Written and unit-tested in Phase 3.4; the static-file handler that calls it
//! lands in TASKS.md 5.1.
#![allow(dead_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

/// Fallback for an unknown extension.
///
/// `application/octet-stream` rather than `text/plain`: guessing "text" for
/// unknown bytes makes a browser try to render a binary file, and lets a file
/// with an unexpected extension be sniffed as something executable.
pub const DEFAULT: &str = "application/octet-stream";

/// Content type for a path, chosen by its extension.
pub fn for_path(path: &std::path::Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    for_extension(&extension)
}

/// Content type for a bare extension, without the dot.
pub fn for_extension(extension: &str) -> &'static str {
    match extension {
        // Markup and text
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" | "text" | "log" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        // Fonts
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        // Media
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        // Documents and archives
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        // Scripts served as downloads rather than executed
        "py" => "text/x-python; charset=utf-8",
        "sh" => "text/x-shellscript; charset=utf-8",
        _ => DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::Path;

    #[test]
    fn maps_the_types_a_static_site_needs() {
        assert_eq!(for_path(Path::new("index.html")), "text/html; charset=utf-8");
        assert_eq!(for_path(Path::new("style.css")), "text/css; charset=utf-8");
        assert_eq!(for_path(Path::new("logo.png")), "image/png");
        assert_eq!(for_path(Path::new("data.csv")), "text/csv; charset=utf-8");
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(for_path(Path::new("PHOTO.JPG")), "image/jpeg");
        assert_eq!(for_path(Path::new("Doc.PDF")), "application/pdf");
    }

    /// Unknown and extensionless files must not be guessed as text: a browser
    /// would try to render them, and sniffing is a security problem.
    #[test]
    fn unknown_and_missing_extensions_fall_back_to_octet_stream() {
        assert_eq!(for_path(Path::new("archive.xyz")), DEFAULT);
        assert_eq!(for_path(Path::new("Makefile")), DEFAULT);
        assert_eq!(for_path(Path::new("noextension")), DEFAULT);
    }

    #[test]
    fn text_types_declare_their_charset() {
        for path in ["a.html", "a.css", "a.js", "a.txt", "a.md", "a.csv"] {
            assert!(
                for_path(Path::new(path)).contains("charset=utf-8"),
                "{path} should declare a charset"
            );
        }
    }

    /// A dotfile's name is not an extension.
    #[test]
    fn a_leading_dot_is_not_an_extension() {
        assert_eq!(for_path(Path::new(".gitignore")), DEFAULT);
    }
}
