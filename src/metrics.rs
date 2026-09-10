//! Server metrics and statistics tracking.
//!
//! Provides the [`Metrics`] registry tracking uptime, active/total connections,
//! request method counters, response status code distributions, I/O byte volumes,
//! open file descriptor count, and CGI process metrics.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Server runtime metrics.
#[derive(Debug)]
pub struct Metrics {
    pub start_time: Instant,
    pub total_connections: u64,
    pub active_connections: usize,
    pub requests_total: u64,
    pub requests_by_method: HashMap<String, u64>,
    pub responses_total: u64,
    /// Indexes 1..=5 map to 1xx..5xx response classes; index 0 catches any out-of-range status.
    pub responses_by_status_class: [u64; 6],
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub cgi_spawned: u64,
    pub cgi_failed: u64,
    pub cgi_timed_out: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Create a new metrics registry with initial timestamps and zeroed counters.
    pub fn new() -> Metrics {
        let mut requests_by_method = HashMap::new();
        requests_by_method.insert("GET".to_string(), 0);
        requests_by_method.insert("POST".to_string(), 0);
        requests_by_method.insert("DELETE".to_string(), 0);
        requests_by_method.insert("HEAD".to_string(), 0);
        requests_by_method.insert("OTHER".to_string(), 0);

        Metrics {
            start_time: Instant::now(),
            total_connections: 0,
            active_connections: 0,
            requests_total: 0,
            requests_by_method,
            responses_total: 0,
            responses_by_status_class: [0; 6],
            bytes_in: 0,
            bytes_out: 0,
            cgi_spawned: 0,
            cgi_failed: 0,
            cgi_timed_out: 0,
        }
    }

    /// Record a newly accepted client connection.
    pub fn record_connection_open(&mut self) {
        self.total_connections = self.total_connections.saturating_add(1);
        self.active_connections = self.active_connections.saturating_add(1);
    }

    /// Record a closed client connection.
    pub fn record_connection_close(&mut self) {
        self.active_connections = self.active_connections.saturating_sub(1);
    }

    /// Record bytes read from client sockets.
    pub fn record_bytes_in(&mut self, n: usize) {
        self.bytes_in = self.bytes_in.saturating_add(n as u64);
    }

    /// Record bytes written to client sockets.
    pub fn record_bytes_out(&mut self, n: usize) {
        self.bytes_out = self.bytes_out.saturating_add(n as u64);
    }

    /// Record a parsed request method (GET, POST, DELETE, HEAD, or OTHER).
    pub fn record_request(&mut self, method: &str) {
        self.requests_total = self.requests_total.saturating_add(1);
        let key = match method.to_ascii_uppercase().as_str() {
            "GET" => "GET",
            "POST" => "POST",
            "DELETE" => "DELETE",
            "HEAD" => "HEAD",
            _ => "OTHER",
        };
        let count = self.requests_by_method.entry(key.to_string()).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Record a response status code and categorize by HTTP status class.
    pub fn record_response(&mut self, status: u16) {
        self.responses_total = self.responses_total.saturating_add(1);
        let class = (status / 100) as usize;
        if (1..=5).contains(&class) {
            self.responses_by_status_class[class] =
                self.responses_by_status_class[class].saturating_add(1);
        } else {
            self.responses_by_status_class[0] =
                self.responses_by_status_class[0].saturating_add(1);
        }
    }

    /// Record a spawned CGI child process.
    pub fn record_cgi_spawn(&mut self) {
        self.cgi_spawned = self.cgi_spawned.saturating_add(1);
    }

    /// Record a failed CGI invocation.
    pub fn record_cgi_failure(&mut self) {
        self.cgi_failed = self.cgi_failed.saturating_add(1);
    }

    /// Record a CGI script that exceeded its execution deadline.
    pub fn record_cgi_timeout(&mut self) {
        self.cgi_timed_out = self.cgi_timed_out.saturating_add(1);
    }

    /// Count open file descriptors via `/proc/self/fd` on Linux (fallback 0).
    pub fn open_fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }

    /// Elapsed time since the server metrics started.
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Serialize current metrics into a hand-crafted JSON string without external dependencies.
    pub fn to_json(&self, active_sessions: usize) -> String {
        let uptime_secs = self.uptime().as_secs();
        let get_cnt = self.requests_by_method.get("GET").copied().unwrap_or(0);
        let post_cnt = self.requests_by_method.get("POST").copied().unwrap_or(0);
        let delete_cnt = self.requests_by_method.get("DELETE").copied().unwrap_or(0);
        let head_cnt = self.requests_by_method.get("HEAD").copied().unwrap_or(0);
        let other_cnt = self.requests_by_method.get("OTHER").copied().unwrap_or(0);

        format!(
            "{{\n  \
              \"uptime_seconds\": {},\n  \
              \"total_connections\": {},\n  \
              \"active_connections\": {},\n  \
              \"requests_total\": {},\n  \
              \"requests_by_method\": {{\n    \
                \"GET\": {},\n    \
                \"POST\": {},\n    \
                \"DELETE\": {},\n    \
                \"HEAD\": {},\n    \
                \"OTHER\": {}\n  \
              }},\n  \
              \"responses_total\": {},\n  \
              \"responses_by_status_class\": {{\n    \
                \"1xx\": {},\n    \
                \"2xx\": {},\n    \
                \"3xx\": {},\n    \
                \"4xx\": {},\n    \
                \"5xx\": {}\n  \
              }},\n  \
              \"bytes_in\": {},\n  \
              \"bytes_out\": {},\n  \
              \"cgi_spawned\": {},\n  \
              \"cgi_failed\": {},\n  \
              \"cgi_timed_out\": {},\n  \
              \"open_fds\": {},\n  \
              \"active_sessions\": {}\n\
            }}\n",
            uptime_secs,
            self.total_connections,
            self.active_connections,
            self.requests_total,
            get_cnt,
            post_cnt,
            delete_cnt,
            head_cnt,
            other_cnt,
            self.responses_total,
            self.responses_by_status_class[1],
            self.responses_by_status_class[2],
            self.responses_by_status_class[3],
            self.responses_by_status_class[4],
            self.responses_by_status_class[5],
            self.bytes_in,
            self.bytes_out,
            self.cgi_spawned,
            self.cgi_failed,
            self.cgi_timed_out,
            Self::open_fd_count(),
            active_sessions,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn initial_metrics_state() {
        let metrics = Metrics::new();
        assert_eq!(metrics.total_connections, 0);
        assert_eq!(metrics.active_connections, 0);
        assert_eq!(metrics.requests_total, 0);
        assert_eq!(metrics.responses_total, 0);
        assert_eq!(metrics.bytes_in, 0);
        assert_eq!(metrics.bytes_out, 0);
        assert_eq!(metrics.cgi_spawned, 0);
        assert_eq!(metrics.cgi_failed, 0);
        assert_eq!(metrics.cgi_timed_out, 0);
        assert_eq!(metrics.requests_by_method.get("GET"), Some(&0));
        assert_eq!(metrics.requests_by_method.get("POST"), Some(&0));
        assert_eq!(metrics.requests_by_method.get("DELETE"), Some(&0));
        assert_eq!(metrics.requests_by_method.get("HEAD"), Some(&0));
        assert_eq!(metrics.requests_by_method.get("OTHER"), Some(&0));
        assert_eq!(metrics.responses_by_status_class, [0; 6]);
    }

    #[test]
    fn connection_tracking() {
        let mut metrics = Metrics::new();
        metrics.record_connection_open();
        metrics.record_connection_open();
        assert_eq!(metrics.total_connections, 2);
        assert_eq!(metrics.active_connections, 2);

        metrics.record_connection_close();
        assert_eq!(metrics.total_connections, 2);
        assert_eq!(metrics.active_connections, 1);

        metrics.record_connection_close();
        assert_eq!(metrics.active_connections, 0);

        // Saturating underflow protection
        metrics.record_connection_close();
        assert_eq!(metrics.active_connections, 0);
    }

    #[test]
    fn bytes_tracking() {
        let mut metrics = Metrics::new();
        metrics.record_bytes_in(1024);
        metrics.record_bytes_in(512);
        assert_eq!(metrics.bytes_in, 1536);

        metrics.record_bytes_out(2048);
        metrics.record_bytes_out(4096);
        assert_eq!(metrics.bytes_out, 6144);
    }

    #[test]
    fn request_tracking() {
        let mut metrics = Metrics::new();
        metrics.record_request("GET");
        metrics.record_request("get");
        metrics.record_request("POST");
        metrics.record_request("DELETE");
        metrics.record_request("HEAD");
        metrics.record_request("PUT");
        metrics.record_request("OPTIONS");

        assert_eq!(metrics.requests_total, 7);
        assert_eq!(metrics.requests_by_method.get("GET"), Some(&2));
        assert_eq!(metrics.requests_by_method.get("POST"), Some(&1));
        assert_eq!(metrics.requests_by_method.get("DELETE"), Some(&1));
        assert_eq!(metrics.requests_by_method.get("HEAD"), Some(&1));
        assert_eq!(metrics.requests_by_method.get("OTHER"), Some(&2));
    }

    #[test]
    fn response_tracking() {
        let mut metrics = Metrics::new();
        metrics.record_response(100);
        metrics.record_response(200);
        metrics.record_response(204);
        metrics.record_response(301);
        metrics.record_response(404);
        metrics.record_response(413);
        metrics.record_response(500);
        metrics.record_response(504);
        metrics.record_response(99); // out of range

        assert_eq!(metrics.responses_total, 9);
        assert_eq!(metrics.responses_by_status_class[1], 1); // 1xx
        assert_eq!(metrics.responses_by_status_class[2], 2); // 2xx
        assert_eq!(metrics.responses_by_status_class[3], 1); // 3xx
        assert_eq!(metrics.responses_by_status_class[4], 2); // 4xx
        assert_eq!(metrics.responses_by_status_class[5], 2); // 5xx
        assert_eq!(metrics.responses_by_status_class[0], 1); // other
    }

    #[test]
    fn cgi_tracking() {
        let mut metrics = Metrics::new();
        metrics.record_cgi_spawn();
        metrics.record_cgi_spawn();
        metrics.record_cgi_failure();
        metrics.record_cgi_timeout();

        assert_eq!(metrics.cgi_spawned, 2);
        assert_eq!(metrics.cgi_failed, 1);
        assert_eq!(metrics.cgi_timed_out, 1);
    }

    #[test]
    fn open_fd_count_sanity() {
        let count = Metrics::open_fd_count();
        // On Linux during test execution, at least stdin, stdout, stderr exist
        #[cfg(target_os = "linux")]
        assert!(count >= 3);
    }

    #[test]
    fn uptime_and_json_serialization() {
        let mut metrics = Metrics::new();
        metrics.record_connection_open();
        metrics.record_request("GET");
        metrics.record_response(200);
        metrics.record_bytes_in(100);
        metrics.record_bytes_out(250);

        assert!(metrics.uptime() <= Duration::from_secs(10));

        let json = metrics.to_json(4);
        assert!(json.contains("\"uptime_seconds\":"));
        assert!(json.contains("\"total_connections\": 1"));
        assert!(json.contains("\"active_connections\": 1"));
        assert!(json.contains("\"requests_total\": 1"));
        assert!(json.contains("\"GET\": 1"));
        assert!(json.contains("\"responses_total\": 1"));
        assert!(json.contains("\"2xx\": 1"));
        assert!(json.contains("\"bytes_in\": 100"));
        assert!(json.contains("\"bytes_out\": 250"));
        assert!(json.contains("\"active_sessions\": 4"));
        assert!(json.contains("\"open_fds\":"));
    }
}
