//! Configuration data model: [`Config`], [`ServerConfig`], [`RouteConfig`].
//!
//! Every field here maps to a bullet in the subject's "Configuration File"
//! section. Nothing about server behaviour is hardcoded elsewhere.
//!
//! Loading is a three-step pipeline, and the split matters at audit time:
//!
//! 1. [`parser`] turns the file text into this data model. Failures here are
//!    *syntax* errors and always carry a line number.
//! 2. [`validate`] checks the model against the filesystem and against itself.
//!    Failures here are *semantic* and split into two severities — see
//!    [`validate::Severity`].
//! 3. Only then does [`crate::server`] bind anything.
//!
//! Relative paths in the config are resolved against the **config file's own
//! directory**, not the process working directory, so the server can be started
//! from anywhere with `localserver /etc/localserver/config.yaml`.

pub mod parser;
pub mod validate;

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Body limit applied when a server block does not set one.
pub const DEFAULT_MAX_BODY: usize = 1024 * 1024;

/// Upper bound accepted for `client_max_body_size`. A limit above this is
/// almost certainly a typo (`1G` where `1M` was meant) and would let a single
/// request pin an unbounded amount of memory.
pub const MAX_BODY_CEILING: usize = 1024 * 1024 * 1024;

/// HTTP methods that may appear in a route's `methods:` list.
///
/// `Put` and `Options` are parsed so a config can mention them, but only
/// `Get`/`Head`/`Post`/`Delete` are implemented by the handlers; anything else
/// answers 501 at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Delete,
    Put,
    Options,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Delete => "DELETE",
            Method::Put => "PUT",
            Method::Options => "OPTIONS",
        }
    }

    /// Case-sensitive, as HTTP methods are. Returns `None` for anything unknown.
    pub fn parse(s: &str) -> Option<Method> {
        match s {
            "GET" => Some(Method::Get),
            "HEAD" => Some(Method::Head),
            "POST" => Some(Method::Post),
            "DELETE" => Some(Method::Delete),
            "PUT" => Some(Method::Put),
            "OPTIONS" => Some(Method::Options),
            _ => None,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `redirect:` block on a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub status: u16,
    pub target: String,
}

/// One `routes:` entry: a URL prefix and what to do with requests under it.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// URL prefix this route claims, e.g. `/uploads`. Always starts with `/`.
    pub path: String,
    /// Methods the route accepts. Anything else answers 405 + `Allow`.
    pub methods: Vec<Method>,
    /// Filesystem directory the URL remainder is resolved against.
    pub root: PathBuf,
    /// File served when the resolved path is a directory.
    pub index: Option<String>,
    /// Generate a listing for a directory with no index file.
    pub autoindex: bool,
    /// When set, the route answers a 3xx and never touches the filesystem.
    pub redirect: Option<Redirect>,
    /// File extension (with the dot) to interpreter, e.g. `.py` -> python3.
    pub cgi: BTreeMap<String, PathBuf>,
    /// Destination directory for uploads accepted by this route.
    pub upload_dir: Option<PathBuf>,
    /// Line in the config file, for error messages.
    pub line: usize,
}

/// One `servers:` entry: a set of listeners plus the routes served on them.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub ports: Vec<u16>,
    /// Virtual host names matched against the request's `Host` header.
    pub server_names: Vec<String>,
    /// Serve requests whose `Host` matches no server on this listener.
    pub is_default: bool,
    pub client_max_body_size: usize,
    pub error_pages: BTreeMap<u16, PathBuf>,
    pub routes: Vec<RouteConfig>,
    pub line: usize,
}

impl ServerConfig {
    /// Does this server answer to `host` (a `Host` header value, port already
    /// stripped)? Host names are case-insensitive per RFC 9110.
    // Used by the router in TASKS.md 4.1; defined here beside the field it reads.
    #[allow(dead_code)]
    pub fn matches_name(&self, host: &str) -> bool {
        self.server_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(host))
    }

    /// The address to bind, with `localhost` mapped to the loopback address.
    pub fn bind_addr(&self) -> Option<IpAddr> {
        if self.host.eq_ignore_ascii_case("localhost") {
            return Some(IpAddr::from([127, 0, 0, 1]));
        }
        self.host.parse().ok()
    }
}

/// A whole configuration file.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

impl Config {
    /// Every distinct `host:port` that must be bound, deduplicated: two virtual
    /// hosts sharing an address share **one** listening socket.
    pub fn listeners(&self) -> Vec<(String, u16)> {
        let mut out: Vec<(String, u16)> = Vec::new();
        for server in &self.servers {
            for &port in &server.ports {
                let key = (server.host.clone(), port);
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
        out
    }

    /// Servers attached to a given listener, in declaration order.
    pub fn servers_on(&self, host: &str, port: u16) -> Vec<&ServerConfig> {
        self.servers
            .iter()
            .filter(|s| s.host == host && s.ports.contains(&port))
            .collect()
    }

    /// Parse, resolve paths and validate a config file.
    ///
    /// Returns the surviving servers together with the non-fatal issues that
    /// were reported along the way (servers dropped because their own
    /// configuration is broken). Fatal problems produce an `Err` instead:
    /// startup must not continue.
    pub fn load(path: &Path) -> Result<(Config, Vec<validate::Issue>), ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError {
            file: path.to_path_buf(),
            kind: ConfigErrorKind::Io(e.to_string()),
        })?;

        // Relative paths resolve against the config file's directory.
        let base = path.parent().unwrap_or_else(|| Path::new("."));

        let parsed = parser::parse_config(&text, base).map_err(|e| ConfigError {
            file: path.to_path_buf(),
            kind: ConfigErrorKind::Parse(e),
        })?;

        match validate::validate(parsed) {
            Ok(v) => Ok((Config { servers: v.servers }, v.issues)),
            Err(fatal) => Err(ConfigError {
                file: path.to_path_buf(),
                kind: ConfigErrorKind::Invalid(fatal),
            }),
        }
    }
}

/// Why a config file could not be used.
#[derive(Debug)]
pub struct ConfigError {
    pub file: PathBuf,
    pub kind: ConfigErrorKind,
}

#[derive(Debug)]
pub enum ConfigErrorKind {
    /// The file could not be read at all.
    Io(String),
    /// Syntax error; always has a line number.
    Parse(parser::ParseError),
    /// Semantic errors severe enough to refuse startup.
    Invalid(Vec<validate::Issue>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = self.file.display();
        match &self.kind {
            ConfigErrorKind::Io(e) => write!(f, "{file}: {e}"),
            ConfigErrorKind::Parse(e) => write!(f, "{file}:{}: {}", e.line, e.message),
            ConfigErrorKind::Invalid(issues) => {
                writeln!(f, "{file}: configuration is unusable:")?;
                for (i, issue) in issues.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    if issue.line > 0 {
                        write!(f, "  {file}:{}: {}", issue.line, issue.message)?;
                    } else {
                        write!(f, "  {}", issue.message)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}
