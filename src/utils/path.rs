//! URL path to filesystem path resolution, and traversal sanitising.
//!
//! Percent-decode, reject `%00`, normalise `.` and `..` segments *before*
//! touching the filesystem, then verify the result is still under the route
//! root — otherwise 403. Normalising first also handles the case where the path
//! does not exist and `canonicalize` would simply fail.
//!
//! `GET /../../etc/passwd` is the test an auditor is most likely to try
//! unprompted.
//!
//! Filled in by TASKS.md 4.3.
