//! Hand-written parser for the indentation-based YAML subset used by
//! `config.yaml`: nested maps, `- ` lists, scalars, `#` comments, quoted
//! strings and size suffixes (`1M`, `512K`).
//!
//! Hand-written rather than `serde_yaml` so the dependency set stays at
//! `mio` + `libc`, and so parse errors carry line numbers.
//!
//! Filled in by TASKS.md 1.2.
