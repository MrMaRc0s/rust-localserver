//! Hand-written parser for the indentation-based YAML subset used by
//! `config.yaml`.
//!
//! Supported: nested mappings, block sequences (`- item`), sequences of
//! mappings (`- key: value` with the rest of the mapping aligned under it),
//! `#` comments, single/double quoted scalars, and size suffixes (`1M`, `512K`).
//! Not supported, because the config does not need them: flow collections
//! (`[a, b]`, `{a: b}`), anchors, multi-document files, block scalars (`|`, `>`)
//! and tab indentation. Each of those is *rejected with a message naming the
//! supported alternative* rather than silently mis-parsed — a config that looks
//! accepted but was read differently than written is the worst outcome here.
//!
//! Hand-written rather than pulling in `serde_yaml` so the dependency set stays
//! at `mio` + `libc`, and so every error carries the line it occurred on — which
//! is what makes a config mistake diagnosable instead of mysterious.
//!
//! Parsing happens in two passes: text to a generic [`Yaml`] tree, then that
//! tree into the typed [`Config`]. The split keeps syntax errors ("expected
//! `key: value`") apart from schema errors ("unknown key `serevr_names`"), and
//! both keep their line numbers.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::{
    Config, Method, Redirect, RouteConfig, ServerConfig, DEFAULT_MAX_BODY,
};

/// A syntax or schema error, with the line it was found on (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { line, message: message.into() })
}

// ---------------------------------------------------------------------------
// Pass 1: text -> generic tree
// ---------------------------------------------------------------------------

/// A node of the parsed document. Mappings keep declaration order (a `Vec`
/// rather than a map) so error messages can report keys in the order written,
/// and so duplicate keys are detectable instead of silently overwriting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yaml {
    Scalar { value: String, line: usize },
    List { items: Vec<Yaml>, line: usize },
    Map { entries: Vec<(String, Yaml)>, line: usize },
}

impl Yaml {
    pub fn line(&self) -> usize {
        match self {
            Yaml::Scalar { line, .. } | Yaml::List { line, .. } | Yaml::Map { line, .. } => *line,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Yaml::Scalar { .. } => "a scalar",
            Yaml::List { .. } => "a list",
            Yaml::Map { .. } => "a mapping",
        }
    }
}

#[derive(Debug, Clone)]
struct Line {
    no: usize,
    indent: usize,
    text: String,
}

/// Strip an unquoted `#` comment. A `#` only starts a comment at the start of
/// the line or after whitespace, so `http://host/#frag` and `"a # b"` survive.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'\\' if in_double => i += 1,
            // A `#` only starts a comment at the start of the line or after
            // whitespace, so `host/#frag` keeps its fragment.
            b'#' if !in_single && !in_double && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t') => {
                return &s[..i];
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// Split `key: value` at the first `:` that is outside quotes and followed by a
/// space or end of line. That rule is what keeps `http://x` and `.py: /usr/bin`
/// from being mis-split.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let b = text.as_bytes();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'\\' if in_double => i += 1,
            b':' if !in_single && !in_double => {
                let next = b.get(i + 1).copied();
                if next.is_none() || next == Some(b' ') {
                    return Some((text[..i].trim(), text[i + 1..].trim()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn unquote(s: &str, line: usize) -> Result<String, ParseError> {
    let t = s.trim();
    let b = t.as_bytes();
    if b.is_empty() {
        return Ok(String::new());
    }
    let quote = b[0];
    if quote != b'"' && quote != b'\'' {
        return Ok(t.to_string());
    }
    if t.len() < 2 || b[t.len() - 1] != quote {
        return err(line, "unterminated quoted string");
    }
    let inner = &t[1..t.len() - 1];
    if quote == b'\'' {
        return Ok(inner.to_string());
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => return err(line, format!("unknown escape sequence `\\{other}`")),
            None => return err(line, "trailing backslash in quoted string"),
        }
    }
    Ok(out)
}

fn is_list_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn tokenize(input: &str) -> Result<Vec<Line>, ParseError> {
    let mut out = Vec::new();
    for (i, raw) in input.lines().enumerate() {
        let no = i + 1;
        let indent = raw.chars().take_while(|c| *c == ' ').count();
        if raw[indent..].starts_with('\t') {
            return err(no, "tabs are not allowed for indentation, use spaces");
        }
        let text = strip_comment(&raw[indent..]).trim_end();
        if text.is_empty() {
            continue;
        }
        // Flow collections are not part of the supported subset. Say so rather
        // than letting `[8080]` fail later as "not a valid port number".
        if let Some((_, value)) = split_key(text) {
            let v = value.trim();
            if (v.starts_with('[') && v.ends_with(']')) || (v.starts_with('{') && v.ends_with('}')) {
                return err(
                    no,
                    format!(
                        "flow-style collections are not supported, write a block list instead:\n\
                         \x20   key:\n\x20     - item      (got `{v}`)"
                    ),
                );
            }
        }
        out.push(Line { no, indent, text: text.to_string() });
    }
    Ok(out)
}

struct Parser {
    lines: Vec<Line>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Line> {
        self.lines.get(self.pos)
    }

    fn parse_node(&mut self, indent: usize) -> Result<Yaml, ParseError> {
        match self.peek() {
            None => Ok(Yaml::Scalar { value: String::new(), line: 0 }),
            Some(l) if is_list_item(&l.text) => self.parse_list(indent),
            Some(_) => self.parse_map(indent),
        }
    }

    fn parse_map(&mut self, indent: usize) -> Result<Yaml, ParseError> {
        let start = self.peek().map(|l| l.no).unwrap_or(0);
        let mut entries: Vec<(String, Yaml)> = Vec::new();
        loop {
            let (no, text) = match self.peek() {
                Some(l) if l.indent == indent && !is_list_item(&l.text) => (l.no, l.text.clone()),
                Some(l) if l.indent > indent => {
                    return err(l.no, "unexpected indentation");
                }
                _ => break,
            };

            let (raw_key, rest) = match split_key(&text) {
                Some(v) => v,
                None => return err(no, format!("expected `key: value`, found `{text}`")),
            };
            let key = unquote(raw_key, no)?;
            if key.is_empty() {
                return err(no, "empty key");
            }
            let rest = rest.to_string();
            self.pos += 1;

            let value = if !rest.is_empty() {
                Yaml::Scalar { value: unquote(&rest, no)?, line: no }
            } else {
                // Value lives on the following lines, either indented deeper
                // (mapping or list) or as a list at the key's own indent.
                match self.peek() {
                    Some(n) if n.indent > indent => {
                        let deeper = n.indent;
                        self.parse_node(deeper)?
                    }
                    Some(n) if n.indent == indent && is_list_item(&n.text) => {
                        self.parse_list(indent)?
                    }
                    _ => Yaml::Scalar { value: String::new(), line: no },
                }
            };

            if entries.iter().any(|(k, _)| *k == key) {
                return err(no, format!("duplicate key `{key}`"));
            }
            entries.push((key, value));
        }
        Ok(Yaml::Map { entries, line: start })
    }

    fn parse_list(&mut self, indent: usize) -> Result<Yaml, ParseError> {
        let start = self.peek().map(|l| l.no).unwrap_or(0);
        let mut items = Vec::new();
        loop {
            let (no, text) = match self.peek() {
                Some(l) if l.indent == indent && is_list_item(&l.text) => (l.no, l.text.clone()),
                _ => break,
            };
            let after = &text[1..];
            let trimmed = after.trim_start();
            let key_col = indent + 1 + (after.len() - trimmed.len());

            if trimmed.is_empty() {
                // `-` alone: the item is the indented block that follows.
                self.pos += 1;
                let value = match self.peek() {
                    Some(n) if n.indent > indent => {
                        let deeper = n.indent;
                        self.parse_node(deeper)?
                    }
                    _ => Yaml::Scalar { value: String::new(), line: no },
                };
                items.push(value);
            } else if split_key(trimmed).is_some() {
                // `- key: value`: rewrite the line as an ordinary mapping line
                // starting at the key's column, then parse the block normally.
                // The remaining keys of the item are aligned to that column.
                self.lines[self.pos].indent = key_col;
                self.lines[self.pos].text = trimmed.to_string();
                items.push(self.parse_map(key_col)?);
            } else {
                self.pos += 1;
                items.push(Yaml::Scalar { value: unquote(trimmed, no)?, line: no });
            }
        }
        Ok(Yaml::List { items, line: start })
    }
}

/// Parse the document into a generic tree. Exposed for unit tests.
pub fn parse_yaml(input: &str) -> Result<Yaml, ParseError> {
    let lines = tokenize(input)?;
    if lines.is_empty() {
        return Ok(Yaml::Map { entries: Vec::new(), line: 0 });
    }
    let base = lines[0].indent;
    let mut parser = Parser { lines, pos: 0 };
    let node = parser.parse_node(base)?;
    if let Some(l) = parser.peek() {
        return err(l.no, format!("unexpected content `{}`", l.text));
    }
    Ok(node)
}

// ---------------------------------------------------------------------------
// Pass 2: generic tree -> typed Config
// ---------------------------------------------------------------------------

fn as_map<'a>(node: &'a Yaml, what: &str) -> Result<&'a [(String, Yaml)], ParseError> {
    match node {
        Yaml::Map { entries, .. } => Ok(entries),
        other => err(other.line(), format!("`{what}` must be a mapping, found {}", other.type_name())),
    }
}

fn as_scalar<'a>(node: &'a Yaml, what: &str) -> Result<&'a str, ParseError> {
    match node {
        Yaml::Scalar { value, .. } => Ok(value),
        other => err(other.line(), format!("`{what}` must be a scalar, found {}", other.type_name())),
    }
}

/// Accept either a list or a single scalar, so `ports: 8080` and a block list
/// both work. Being lenient here removes a whole class of confusing config
/// mistakes without making anything ambiguous.
fn as_list<'a>(node: &'a Yaml, what: &str) -> Result<Vec<&'a Yaml>, ParseError> {
    match node {
        Yaml::List { items, .. } => Ok(items.iter().collect()),
        Yaml::Scalar { .. } => Ok(vec![node]),
        other => err(other.line(), format!("`{what}` must be a list, found {}", other.type_name())),
    }
}

fn get<'a>(entries: &'a [(String, Yaml)], key: &str) -> Option<&'a Yaml> {
    entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Reject unknown keys instead of ignoring them: a silently ignored `serevr_names`
/// looks exactly like a server that does not work, and costs far more to debug
/// than an error at startup.
fn check_keys(
    entries: &[(String, Yaml)],
    allowed: &[&str],
    line: usize,
    what: &str,
) -> Result<(), ParseError> {
    for (key, value) in entries {
        if !allowed.contains(&key.as_str()) {
            let at = if value.line() > 0 { value.line() } else { line };
            return err(
                at,
                format!("unknown key `{key}` in {what}; expected one of: {}", allowed.join(", ")),
            );
        }
    }
    Ok(())
}

fn parse_bool(node: &Yaml, what: &str) -> Result<bool, ParseError> {
    let s = as_scalar(node, what)?;
    match s.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        _ => err(node.line(), format!("`{what}` must be true or false, found `{s}`")),
    }
}

fn parse_u16(node: &Yaml, what: &str) -> Result<u16, ParseError> {
    let s = as_scalar(node, what)?;
    s.parse::<u16>()
        .map_err(|_| ParseError { line: node.line(), message: format!("`{what}`: `{s}` is not a valid port number (0-65535)") })
}

/// `1024`, `512K`, `1M`, `2G` (also `KB`/`MB`/`GB`), case-insensitive.
fn parse_size(node: &Yaml, what: &str) -> Result<usize, ParseError> {
    let s = as_scalar(node, what)?.trim();
    if s.is_empty() {
        return err(node.line(), format!("`{what}` is empty"));
    }
    let upper = s.to_ascii_uppercase();
    let (digits, multiplier) = if let Some(d) = upper.strip_suffix("KB").or_else(|| upper.strip_suffix('K')) {
        (d, 1024usize)
    } else if let Some(d) = upper.strip_suffix("MB").or_else(|| upper.strip_suffix('M')) {
        (d, 1024 * 1024)
    } else if let Some(d) = upper.strip_suffix("GB").or_else(|| upper.strip_suffix('G')) {
        (d, 1024 * 1024 * 1024)
    } else {
        (upper.as_str(), 1usize)
    };
    let digits = digits.trim();
    let n: usize = digits.parse().map_err(|_| ParseError {
        line: node.line(),
        message: format!("`{what}`: `{s}` is not a size (expected e.g. 1048576, 512K, 1M)"),
    })?;
    n.checked_mul(multiplier).ok_or_else(|| ParseError {
        line: node.line(),
        message: format!("`{what}`: `{s}` overflows"),
    })
}

fn resolve(base: &Path, value: &str) -> PathBuf {
    let p = Path::new(value);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

const SERVER_KEYS: &[&str] = &[
    "host",
    "ports",
    "server_names",
    "is_default",
    "client_max_body_size",
    "error_pages",
    "routes",
];

const ROUTE_KEYS: &[&str] = &[
    "path",
    "methods",
    "root",
    "index",
    "autoindex",
    "redirect",
    "cgi",
    "upload_dir",
];

fn route_from_yaml(node: &Yaml, base: &Path) -> Result<RouteConfig, ParseError> {
    let line = node.line();
    let entries = as_map(node, "route")?;
    check_keys(entries, ROUTE_KEYS, line, "a route")?;

    let path = match get(entries, "path") {
        Some(n) => as_scalar(n, "path")?.to_string(),
        None => return err(line, "route is missing `path`"),
    };

    let methods = match get(entries, "methods") {
        Some(n) => {
            let mut out = Vec::new();
            for item in as_list(n, "methods")? {
                let raw = as_scalar(item, "methods")?;
                match Method::parse(raw) {
                    Some(m) if !out.contains(&m) => out.push(m),
                    Some(_) => {}
                    None => return err(item.line(), format!("unknown HTTP method `{raw}`")),
                }
            }
            out
        }
        // A route that does not say otherwise serves reads only.
        None => vec![Method::Get, Method::Head],
    };

    let redirect = match get(entries, "redirect") {
        Some(n) => {
            let rentries = as_map(n, "redirect")?;
            check_keys(rentries, &["status", "target"], n.line(), "a redirect")?;
            let target = match get(rentries, "target") {
                Some(t) => as_scalar(t, "target")?.to_string(),
                None => return err(n.line(), "`redirect` is missing `target`"),
            };
            let status = match get(rentries, "status") {
                Some(s) => {
                    let raw = as_scalar(s, "status")?;
                    raw.parse::<u16>().map_err(|_| ParseError {
                        line: s.line(),
                        message: format!("`status`: `{raw}` is not a status code"),
                    })?
                }
                None => 302,
            };
            Some(Redirect { status, target })
        }
        None => None,
    };

    // A pure redirect route never touches the filesystem, so `root` is optional
    // there and defaults to the config directory.
    let root = match get(entries, "root") {
        Some(n) => resolve(base, as_scalar(n, "root")?),
        None if redirect.is_some() => base.to_path_buf(),
        None => return err(line, "route is missing `root`"),
    };

    let index = match get(entries, "index") {
        Some(n) => Some(as_scalar(n, "index")?.to_string()),
        None => None,
    };

    let autoindex = match get(entries, "autoindex") {
        Some(n) => parse_bool(n, "autoindex")?,
        None => false,
    };

    let mut cgi = BTreeMap::new();
    if let Some(n) = get(entries, "cgi") {
        for (ext, interp) in as_map(n, "cgi")? {
            let interpreter = as_scalar(interp, "cgi")?;
            cgi.insert(ext.clone(), resolve(base, interpreter));
        }
    }

    let upload_dir = match get(entries, "upload_dir") {
        Some(n) => Some(resolve(base, as_scalar(n, "upload_dir")?)),
        None => None,
    };

    Ok(RouteConfig { path, methods, root, index, autoindex, redirect, cgi, upload_dir, line })
}

fn server_from_yaml(node: &Yaml, base: &Path) -> Result<ServerConfig, ParseError> {
    let line = node.line();
    let entries = as_map(node, "server")?;
    check_keys(entries, SERVER_KEYS, line, "a server")?;

    let host = match get(entries, "host") {
        Some(n) => as_scalar(n, "host")?.to_string(),
        None => "127.0.0.1".to_string(),
    };

    let ports = match get(entries, "ports") {
        Some(n) => {
            let mut out = Vec::new();
            for item in as_list(n, "ports")? {
                out.push(parse_u16(item, "ports")?);
            }
            out
        }
        None => return err(line, "server is missing `ports`"),
    };

    let server_names = match get(entries, "server_names") {
        Some(n) => {
            let mut out = Vec::new();
            for item in as_list(n, "server_names")? {
                out.push(as_scalar(item, "server_names")?.to_ascii_lowercase());
            }
            out
        }
        None => Vec::new(),
    };

    let is_default = match get(entries, "is_default") {
        Some(n) => parse_bool(n, "is_default")?,
        None => false,
    };

    let client_max_body_size = match get(entries, "client_max_body_size") {
        Some(n) => parse_size(n, "client_max_body_size")?,
        None => DEFAULT_MAX_BODY,
    };

    let mut error_pages = BTreeMap::new();
    if let Some(n) = get(entries, "error_pages") {
        for (code, page) in as_map(n, "error_pages")? {
            let parsed: u16 = code.parse().map_err(|_| ParseError {
                line: page.line(),
                message: format!("`error_pages`: `{code}` is not a status code"),
            })?;
            if !(400..=599).contains(&parsed) {
                return err(page.line(), format!("`error_pages`: {parsed} is not a 4xx or 5xx status"));
            }
            error_pages.insert(parsed, resolve(base, as_scalar(page, "error_pages")?));
        }
    }

    let routes = match get(entries, "routes") {
        Some(n) => {
            let mut out = Vec::new();
            for item in as_list(n, "routes")? {
                out.push(route_from_yaml(item, base)?);
            }
            out
        }
        None => return err(line, "server is missing `routes`"),
    };

    Ok(ServerConfig {
        host,
        ports,
        server_names,
        is_default,
        client_max_body_size,
        error_pages,
        routes,
        line,
    })
}

/// Parse config text into the typed model. Relative paths are resolved against
/// `base` (the config file's directory).
pub fn parse_config(text: &str, base: &Path) -> Result<Config, ParseError> {
    let doc = parse_yaml(text)?;
    let entries = as_map(&doc, "document")?;
    if entries.is_empty() {
        return err(1, "configuration file is empty");
    }
    check_keys(entries, &["servers"], doc.line(), "the document")?;

    let servers_node = match get(entries, "servers") {
        Some(n) => n,
        None => return err(doc.line().max(1), "configuration must define `servers`"),
    };

    let mut servers = Vec::new();
    for item in as_list(servers_node, "servers")? {
        servers.push(server_from_yaml(item, base)?);
    }
    if servers.is_empty() {
        return err(servers_node.line().max(1), "`servers` must define at least one server");
    }
    Ok(Config { servers })
}

// ---------------------------------------------------------------------------
// Tests — audit question [A-37] (configuration validation) and the subject's
// "Configuration Validation" unit-test requirement. Parsing side.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ParseError> {
        parse_config(text, Path::new("."))
    }

    /// Minimal valid document, used as a base for the error cases below.
    const MINIMAL: &str = "\
servers:
  - host: 127.0.0.1
    ports:
      - 8080
    routes:
      - path: /
        root: www
";

    #[test]
    fn parses_nested_maps_and_lists() {
        let doc = parse_yaml("a: 1\nb:\n  c: 2\n  d:\n    - x\n    - y\n").unwrap();
        let entries = match &doc {
            Yaml::Map { entries, .. } => entries,
            other => panic!("expected a map, got {other:?}"),
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(as_scalar(get(entries, "a").unwrap(), "a").unwrap(), "1");

        let b = as_map(get(entries, "b").unwrap(), "b").unwrap();
        assert_eq!(as_scalar(get(b, "c").unwrap(), "c").unwrap(), "2");
        let d = as_list(get(b, "d").unwrap(), "d").unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(as_scalar(d[1], "d").unwrap(), "y");
    }

    /// `- key: value` with the rest of the mapping aligned under it. This is the
    /// shape every server and route in config.yaml uses.
    #[test]
    fn parses_sequence_of_mappings() {
        let doc = parse_yaml("items:\n  - name: first\n    size: 1\n  - name: second\n    size: 2\n").unwrap();
        let entries = match &doc {
            Yaml::Map { entries, .. } => entries,
            other => panic!("expected a map, got {other:?}"),
        };
        let items = as_list(get(entries, "items").unwrap(), "items").unwrap();
        assert_eq!(items.len(), 2);
        let second = as_map(items[1], "item").unwrap();
        assert_eq!(as_scalar(get(second, "name").unwrap(), "name").unwrap(), "second");
        assert_eq!(as_scalar(get(second, "size").unwrap(), "size").unwrap(), "2");
    }

    #[test]
    fn strips_comments_but_not_inside_quotes_or_urls() {
        assert_eq!(strip_comment("a: 1 # trailing"), "a: 1 ");
        assert_eq!(strip_comment("# whole line"), "");
        assert_eq!(strip_comment(r#"a: "has # inside""#), r#"a: "has # inside""#);
        // A fragment is not a comment: no whitespace before the `#`.
        assert_eq!(strip_comment("url: http://host/page#frag"), "url: http://host/page#frag");
    }

    #[test]
    fn splits_keys_without_breaking_urls_or_quoted_keys() {
        assert_eq!(split_key("path: /uploads"), Some(("path", "/uploads")));
        assert_eq!(split_key("target: http://example.com:8080/x"), Some(("target", "http://example.com:8080/x")));
        assert_eq!(split_key(r#"".py": /usr/bin/python3"#), Some((r#"".py""#, "/usr/bin/python3")));
        assert_eq!(split_key("no colon here"), None);
        // `key:` with nothing after it introduces a block value.
        assert_eq!(split_key("routes:"), Some(("routes", "")));
    }

    #[test]
    fn parses_quoted_scalars_and_escapes() {
        assert_eq!(unquote(r#""plain""#, 1).unwrap(), "plain");
        assert_eq!(unquote("'single'", 1).unwrap(), "single");
        assert_eq!(unquote(r#""a\tb""#, 1).unwrap(), "a\tb");
        assert_eq!(unquote("bare", 1).unwrap(), "bare");
        assert_eq!(unquote(r#""""#, 1).unwrap(), "");
        assert!(unquote(r#""unterminated"#, 1).is_err());
    }

    #[test]
    fn parses_size_suffixes() {
        let size = |s: &str| {
            let node = Yaml::Scalar { value: s.to_string(), line: 1 };
            parse_size(&node, "size")
        };
        assert_eq!(size("1024").unwrap(), 1024);
        assert_eq!(size("1K").unwrap(), 1024);
        assert_eq!(size("512k").unwrap(), 512 * 1024);
        assert_eq!(size("1M").unwrap(), 1024 * 1024);
        assert_eq!(size("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(size("1G").unwrap(), 1024 * 1024 * 1024);
        assert!(size("banana").is_err());
        assert!(size("").is_err());
    }

    #[test]
    fn parses_boolean_spellings() {
        let b = |s: &str| parse_bool(&Yaml::Scalar { value: s.to_string(), line: 1 }, "flag");
        assert!(b("true").unwrap());
        assert!(b("TRUE").unwrap());
        assert!(b("yes").unwrap());
        assert!(b("on").unwrap());
        assert!(!b("false").unwrap());
        assert!(!b("off").unwrap());
        assert!(b("maybe").is_err());
    }

    #[test]
    fn full_config_round_trips() {
        let text = "\
servers:
  - host: 127.0.0.1
    ports:
      - 8080
      - 8081
    server_names:
      - Example.COM
    is_default: true
    client_max_body_size: 2M
    error_pages:
      404: pages/404.html
    routes:
      - path: /files
        methods:
          - GET
          - DELETE
        root: www
        index: index.html
        autoindex: true
        upload_dir: uploads
        cgi:
          \".py\": /usr/bin/python3
      - path: /old
        redirect:
          status: 301
          target: /new
";
        let config = parse(text).unwrap();
        assert_eq!(config.servers.len(), 1);
        let server = &config.servers[0];
        assert_eq!(server.ports, vec![8080, 8081]);
        // Server names are lowercased at parse time so Host matching is cheap.
        assert_eq!(server.server_names, vec!["example.com"]);
        assert!(server.is_default);
        assert_eq!(server.client_max_body_size, 2 * 1024 * 1024);
        assert_eq!(server.error_pages.get(&404).unwrap(), Path::new("./pages/404.html"));
        assert_eq!(server.routes.len(), 2);

        let files = &server.routes[0];
        assert_eq!(files.methods, vec![Method::Get, Method::Delete]);
        assert_eq!(files.index.as_deref(), Some("index.html"));
        assert!(files.autoindex);
        assert_eq!(files.cgi.get(".py").unwrap(), Path::new("/usr/bin/python3"));
        assert_eq!(files.upload_dir.as_deref(), Some(Path::new("./uploads")));

        let old = &server.routes[1];
        let redirect = old.redirect.as_ref().unwrap();
        assert_eq!(redirect.status, 301);
        assert_eq!(redirect.target, "/new");
        // A redirect route needs no root, and never reads one.
        assert!(!old.root.as_os_str().is_empty());
    }

    #[test]
    fn accepts_a_single_port_scalar() {
        let config = parse("servers:\n  - ports: 8080\n    routes:\n      - path: /\n        root: www\n").unwrap();
        assert_eq!(config.servers[0].ports, vec![8080]);
        // host defaults to loopback when omitted.
        assert_eq!(config.servers[0].host, "127.0.0.1");
    }

    #[test]
    fn defaults_methods_to_read_only() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.servers[0].routes[0].methods, vec![Method::Get, Method::Head]);
    }

    // --- error cases: every one must report the offending line ---------------

    #[test]
    fn duplicate_key_is_rejected_with_its_line() {
        let text = "servers:\n  - ports: 8080\n    ports: 8081\n    routes:\n      - path: /\n        root: www\n";
        let e = parse(text).unwrap_err();
        assert_eq!(e.line, 3, "should point at the second `ports`");
        assert!(e.message.contains("duplicate key"), "got: {}", e.message);
    }

    #[test]
    fn unknown_key_is_rejected_with_its_line() {
        let text = "servers:\n  - ports: 8080\n    serevr_names:\n      - typo\n    routes:\n      - path: /\n        root: www\n";
        let e = parse(text).unwrap_err();
        assert_eq!(e.line, 4);
        assert!(e.message.contains("unknown key `serevr_names`"), "got: {}", e.message);
    }

    #[test]
    fn flow_style_collections_are_rejected_with_a_hint() {
        let e = parse("servers:\n  - ports: [8080, 8081]\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("flow-style"), "got: {}", e.message);
        assert!(e.message.contains("block list"), "message should name the alternative");
    }

    #[test]
    fn tab_indentation_is_rejected() {
        let e = parse("servers:\n\t- ports: 8080\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("tabs"), "got: {}", e.message);
    }

    #[test]
    fn unknown_method_is_rejected_with_its_line() {
        let text = "servers:\n  - ports: 8080\n    routes:\n      - path: /\n        root: www\n        methods:\n          - GET\n          - FETCH\n";
        let e = parse(text).unwrap_err();
        assert_eq!(e.line, 8);
        assert!(e.message.contains("unknown HTTP method `FETCH`"), "got: {}", e.message);
    }

    #[test]
    fn missing_required_fields_are_reported() {
        let no_ports = parse("servers:\n  - host: 127.0.0.1\n    routes:\n      - path: /\n        root: www\n").unwrap_err();
        assert!(no_ports.message.contains("missing `ports`"), "got: {}", no_ports.message);

        let no_routes = parse("servers:\n  - ports: 8080\n").unwrap_err();
        assert!(no_routes.message.contains("missing `routes`"), "got: {}", no_routes.message);

        let no_path = parse("servers:\n  - ports: 8080\n    routes:\n      - root: www\n").unwrap_err();
        assert!(no_path.message.contains("missing `path`"), "got: {}", no_path.message);

        // A non-redirect route must say where its files live.
        let no_root = parse("servers:\n  - ports: 8080\n    routes:\n      - path: /\n").unwrap_err();
        assert!(no_root.message.contains("missing `root`"), "got: {}", no_root.message);
    }

    #[test]
    fn bad_port_and_status_values_are_reported() {
        let port = parse("servers:\n  - ports: 99999\n    routes:\n      - path: /\n        root: www\n").unwrap_err();
        assert_eq!(port.line, 2);
        assert!(port.message.contains("not a valid port"), "got: {}", port.message);

        let code = parse("servers:\n  - ports: 8080\n    error_pages:\n      999: x.html\n    routes:\n      - path: /\n        root: www\n").unwrap_err();
        assert!(code.message.contains("not a 4xx or 5xx"), "got: {}", code.message);
    }

    #[test]
    fn empty_document_is_rejected() {
        assert!(parse("").is_err());
        assert!(parse("# only a comment\n").is_err());
    }

    #[test]
    fn wrong_node_types_are_reported() {
        // `servers` as a mapping rather than a list of servers.
        let e = parse("servers:\n  host: 127.0.0.1\n").unwrap_err();
        assert!(e.message.contains("must be a") || e.message.contains("unknown key"), "got: {}", e.message);
    }

    /// The config shipped in the repository must always parse. This is the
    /// regression test that catches a syntax the parser cannot handle being
    /// introduced into the real file.
    #[test]
    fn shipped_config_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join("config.yaml")).unwrap();
        let config = parse_config(&text, root).unwrap();
        assert_eq!(config.servers.len(), 3, "config.yaml should declare three servers");

        // Two servers share 127.0.0.1:8080 — virtual hosting, not a conflict.
        let on_8080 = config.servers_on("127.0.0.1", 8080);
        assert_eq!(on_8080.len(), 2);
        assert!(on_8080.iter().any(|s| s.matches_name("localhost")));
        assert!(on_8080.iter().any(|s| s.matches_name("test.com")));

        // One socket per host:port even though four (server, port) pairs exist.
        assert_eq!(config.listeners().len(), 3);
    }
}
