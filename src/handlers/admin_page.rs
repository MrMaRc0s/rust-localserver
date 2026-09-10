//! Admin dashboard and metrics endpoints.
//!
//! Exposes live runtime metrics at `/admin` (interactive auto-refreshing HTML dashboard)
//! and `/admin/metrics.json` (machine-readable JSON).

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use crate::handlers::{human_size, Reply};
use crate::http::response::Response;
use crate::metrics::Metrics;

/// URL prefix owned by the admin dashboard.
pub const PREFIX: &str = "/admin";

/// Does this path belong to the admin dashboard or metrics endpoint?
pub fn owns(path: &str) -> bool {
    path == PREFIX || path == "/admin/" || path == "/admin/metrics.json"
}

/// Serve live metrics in JSON format.
pub fn metrics_json(metrics: &Metrics, active_sessions: usize) -> Reply {
    let json = metrics.to_json(active_sessions);
    Reply::immediate(Response::with_body(
        200,
        "application/json",
        json.into_bytes(),
    ))
}

/// Format a duration into a human-readable string (e.g., "1d 2h 3m 4s", "5m 20s", or "12s").
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m {seconds}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Calculate a formatted percentage string.
fn format_pct(count: u64, total: u64) -> String {
    if total == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", (count as f64 / total as f64) * 100.0)
    }
}

/// Serve the HTML admin dashboard with auto-refresh and statistics cards.
pub fn page(metrics: &Metrics, active_sessions: usize) -> Reply {
    let uptime_str = format_duration(metrics.uptime());
    let open_fds = Metrics::open_fd_count();
    let bytes_in_str = human_size(metrics.bytes_in);
    let bytes_out_str = human_size(metrics.bytes_out);

    let methods = ["GET", "POST", "DELETE", "HEAD", "OTHER"];
    let mut method_rows = String::new();
    for &m in &methods {
        let cnt = metrics.requests_by_method.get(m).copied().unwrap_or(0);
        let pct = format_pct(cnt, metrics.requests_total);
        method_rows.push_str(&format!(
            "<tr><td><code>{m}</code></td><td class=\"num\">{cnt}</td><td class=\"num\">{pct}</td></tr>\n"
        ));
    }

    let status_classes = [
        ("1xx Informational", metrics.responses_by_status_class[1]),
        ("2xx Success", metrics.responses_by_status_class[2]),
        ("3xx Redirection", metrics.responses_by_status_class[3]),
        ("4xx Client Error", metrics.responses_by_status_class[4]),
        ("5xx Server Error", metrics.responses_by_status_class[5]),
    ];
    let mut status_rows = String::new();
    for (label, cnt) in status_classes {
        let pct = format_pct(cnt, metrics.responses_total);
        status_rows.push_str(&format!(
            "<tr><td>{label}</td><td class=\"num\">{cnt}</td><td class=\"num\">{pct}</td></tr>\n"
        ));
    }

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta http-equiv=\"refresh\" content=\"3\">\n\
         <title>Admin Dashboard — LocalServer</title>\n\
         <style>\n\
         body{{margin:0;padding:2rem 1.5rem;background:#10151c;color:#e6edf3;\
         font:15px/1.6 system-ui,-apple-system,\"Segoe UI\",sans-serif}}\n\
         main{{max-width:54rem;margin:0 auto}}\n\
         header{{display:flex;justify-content:space-between;align-items:baseline;\
         margin-bottom:1.5rem;flex-wrap:wrap;gap:1rem}}\n\
         h1{{margin:0;font-size:1.5rem}}\n\
         .subtitle{{margin:.25rem 0 0;color:#8b98a5;font-size:.9rem}}\n\
         .nav a{{color:#58a6ff;text-decoration:none;margin-left:1rem;font-size:.9rem}}\n\
         .nav a:hover{{text-decoration:underline}}\n\
         .grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(12rem,1fr));gap:1rem;margin:1.5rem 0}}\n\
         .card{{background:#182029;border:1px solid #2a3441;border-radius:8px;padding:1rem}}\n\
         .card .label{{color:#8b98a5;font-size:.8rem;text-transform:uppercase;letter-spacing:.04em;margin-bottom:.25rem}}\n\
         .card .val{{font-size:1.5rem;font-weight:600;font-family:ui-monospace,Menlo,monospace;color:#58a6ff}}\n\
         .card .sub{{color:#8b98a5;font-size:.8rem;margin-top:.25rem}}\n\
         h2{{font-size:1.15rem;margin:2rem 0 .75rem}}\n\
         table{{width:100%;border-collapse:collapse;margin:.5rem 0 1.5rem}}\n\
         th,td{{text-align:left;padding:.45rem .6rem;border-bottom:1px solid #2a3441}}\n\
         th{{color:#8b98a5;font-weight:600;font-size:.8rem;text-transform:uppercase;letter-spacing:.04em}}\n\
         td.num{{text-align:right;font-family:ui-monospace,Menlo,monospace}}\n\
         code{{font-family:ui-monospace,Menlo,monospace;font-size:.9em}}\n\
         footer{{margin-top:2.5rem;padding-top:1rem;border-top:1px solid #2a3441;color:#8b98a5;font-size:.85rem}}\n\
         </style></head>\n<body><main>\n\
         <header>\n\
         <div>\n\
         <h1>Admin Dashboard</h1>\n\
         <p class=\"subtitle\">LocalServer live metrics · Auto-refreshing every 3 seconds</p>\n\
         </div>\n\
         <div class=\"nav\">\n\
         <a href=\"/admin/metrics.json\">JSON API</a>\n\
         <a href=\"/session\">Sessions</a>\n\
         <a href=\"/\">Home</a>\n\
         </div>\n\
         </header>\n\
         <div class=\"grid\">\n\
         <div class=\"card\"><div class=\"label\">Uptime</div><div class=\"val\">{uptime_str}</div><div class=\"sub\">Server start</div></div>\n\
         <div class=\"card\"><div class=\"label\">Active Connections</div><div class=\"val\">{active_conns}</div><div class=\"sub\">{total_conns} total accepted</div></div>\n\
         <div class=\"card\"><div class=\"label\">Total Requests</div><div class=\"val\">{req_total}</div><div class=\"sub\">{resp_total} responses</div></div>\n\
         <div class=\"card\"><div class=\"label\">Traffic In / Out</div><div class=\"val\">{bytes_in_str}</div><div class=\"sub\">Out: {bytes_out_str}</div></div>\n\
         <div class=\"card\"><div class=\"label\">Open FDs</div><div class=\"val\">{open_fds}</div><div class=\"sub\">/proc/self/fd descriptors</div></div>\n\
         <div class=\"card\"><div class=\"label\">Active Sessions</div><div class=\"val\">{active_sessions}</div><div class=\"sub\">In-memory store</div></div>\n\
         <div class=\"card\"><div class=\"label\">CGI Processes</div><div class=\"val\">{cgi_spawned}</div><div class=\"sub\">{cgi_failed} failed · {cgi_timed_out} timed out</div></div>\n\
         </div>\n\
         <h2>Requests by Method</h2>\n\
         <table><thead><tr><th>Method</th><th class=\"num\">Count</th><th class=\"num\">Share</th></tr></thead>\n\
         <tbody>\n{method_rows}</tbody></table>\n\
         <h2>Responses by Status Class</h2>\n\
         <table><thead><tr><th>Status Class</th><th class=\"num\">Count</th><th class=\"num\">Share</th></tr></thead>\n\
         <tbody>\n{status_rows}</tbody></table>\n\
         <footer>LocalServer Task 8.1 / Audit A-40 Admin Dashboard · Metrics snapshot</footer>\n\
         </main></body></html>\n",
        uptime_str = uptime_str,
        active_conns = metrics.active_connections,
        total_conns = metrics.total_connections,
        req_total = metrics.requests_total,
        resp_total = metrics.responses_total,
        bytes_in_str = bytes_in_str,
        bytes_out_str = bytes_out_str,
        open_fds = open_fds,
        active_sessions = active_sessions,
        cgi_spawned = metrics.cgi_spawned,
        cgi_failed = metrics.cgi_failed,
        cgi_timed_out = metrics.cgi_timed_out,
        method_rows = method_rows,
        status_rows = status_rows,
    );

    Reply::immediate(Response::with_body(
        200,
        "text/html; charset=utf-8",
        html.into_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn claims_only_admin_paths() {
        assert!(owns("/admin"));
        assert!(owns("/admin/"));
        assert!(owns("/admin/metrics.json"));
        assert!(!owns("/admin/subpath"));
        assert!(!owns("/admin_page"));
        assert!(!owns("/administrator"));
        assert!(!owns("/"));
        assert!(!owns("/session"));
    }

    #[test]
    fn metrics_json_response() {
        let mut metrics = Metrics::new();
        metrics.record_connection_open();
        metrics.record_request("GET");
        metrics.record_response(200);

        let reply = metrics_json(&metrics, 2);
        assert_eq!(reply.response.status, 200);
        assert_eq!(
            reply.response.get_header("Content-Type"),
            Some("application/json")
        );

        let body = String::from_utf8_lossy(&reply.response.body);
        assert!(body.contains("\"uptime_seconds\":"));
        assert!(body.contains("\"active_sessions\": 2"));
        assert!(body.contains("\"requests_total\": 1"));
        assert!(body.contains("\"GET\": 1"));
        assert!(body.contains("\"2xx\": 1"));
    }

    #[test]
    fn admin_page_html_response() {
        let mut metrics = Metrics::new();
        metrics.record_connection_open();
        metrics.record_connection_open();
        metrics.record_request("POST");
        metrics.record_response(201);
        metrics.record_cgi_spawn();
        metrics.record_cgi_timeout();

        let reply = page(&metrics, 5);
        assert_eq!(reply.response.status, 200);
        assert_eq!(
            reply.response.get_header("Content-Type"),
            Some("text/html; charset=utf-8")
        );

        let html = String::from_utf8_lossy(&reply.response.body);
        assert!(html.contains("<meta http-equiv=\"refresh\" content=\"3\">"));
        assert!(html.contains("Admin Dashboard"));
        assert!(html.contains("Active Connections"));
        assert!(html.contains("Requests by Method"));
        assert!(html.contains("Responses by Status Class"));
        assert!(html.contains("POST"));
        assert!(html.contains("2xx Success"));
        assert!(html.contains("5")); // active sessions
        assert!(html.contains("/admin/metrics.json"));
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(3665)), "1h 1m 5s");
        assert_eq!(
            format_duration(Duration::from_secs(90065)),
            "1d 1h 1m 5s"
        );
    }
}
