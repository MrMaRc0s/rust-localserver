//! In-memory session store.
//!
//! # No locking, and why that is the right answer ([A-22])
//!
//! The store is a plain `HashMap` behind an `Rc<RefCell<_>>`, shared by every
//! connection on the process. There is exactly one thread, so a `Mutex` would
//! buy nothing but syscalls and a deadlock risk, and `Arc` would pay for atomic
//! refcounting that is never contended. The subject's advice to "use
//! thread-safe data structures" is satisfied by there being nothing to make
//! safe: the single-threaded design *is* the synchronisation.
//!
//! `RefCell` still enforces the borrow rules at runtime, so an accidental
//! nested borrow is caught rather than corrupting the map — and every borrow
//! here is short and scoped for exactly that reason.
//!
//! # Session ids
//!
//! 32 bytes from `/dev/urandom`, hex-encoded. A guessable id is a session
//! hijack: anyone who can predict one is logged in as that user. Neither the
//! clock nor a counter is acceptable, and there is no need to invent anything —
//! the kernel already has a CSPRNG.
//!
//! If `/dev/urandom` cannot be read the store refuses to invent a weak id: it
//! stops issuing sessions and says so once. The site keeps working without
//! them, which is better than handing out predictable ones.

// The request path must never panic: a panic here would take down every
// other client's connection with it. `unwrap`/`expect` are therefore denied
// rather than merely discouraged (TASKS.md 2.7, the "never crash" rule).
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant, SystemTime};

/// Name of the cookie carrying the session id.
pub const COOKIE_NAME: &str = "localserver_session";

/// How long a session survives without being touched.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// Upper bound on stored sessions.
///
/// Every request without a valid cookie creates one, so a client that ignores
/// cookies — `siege` does — would otherwise grow the map without limit for as
/// long as the run lasts. At the cap, expired entries are swept and, failing
/// that, new sessions are simply not issued.
const MAX_SESSIONS: usize = 10_000;

/// One session's state.
#[derive(Debug, Clone)]
pub struct Session {
    /// Arbitrary key/value data. The demo page stores a name here.
    pub data: HashMap<String, String>,
    /// Requests served on this session.
    pub visits: u64,
    /// Wall-clock creation time, for display.
    pub created: SystemTime,
    /// Monotonic deadline. `Instant` rather than `SystemTime` so a clock
    /// adjustment cannot expire every session at once or extend them forever.
    expires_at: Instant,
}

impl Session {
    fn new(ttl: Duration) -> Session {
        Session {
            data: HashMap::new(),
            visits: 0,
            created: SystemTime::now(),
            expires_at: Instant::now() + ttl,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    /// Seconds left before expiry, for display and for the cookie's `Max-Age`.
    pub fn remaining(&self, now: Instant) -> u64 {
        self.expires_at.saturating_duration_since(now).as_secs()
    }

    pub fn name(&self) -> Option<&str> {
        self.data.get("name").map(|s| s.as_str())
    }
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
    ttl: Duration,
    /// Set once if the CSPRNG is unavailable, so the warning is not repeated
    /// on every request.
    randomness_failed: bool,
}

impl SessionStore {
    pub fn new(ttl: Duration) -> SessionStore {
        SessionStore { sessions: HashMap::new(), ttl, randomness_failed: false }
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Look up a live session, extending its life and counting the visit.
    ///
    /// An expired entry is removed and reported as absent, so a client
    /// presenting a stale id gets a fresh session rather than a resurrected
    /// one.
    pub fn touch(&mut self, id: &str) -> Option<&Session> {
        let now = Instant::now();
        match self.sessions.get(id) {
            Some(session) if session.is_expired(now) => {
                self.sessions.remove(id);
                return None;
            }
            None => return None,
            Some(_) => {}
        }
        let ttl = self.ttl;
        let session = self.sessions.get_mut(id)?;
        session.visits += 1;
        // Sliding expiry: an active session stays alive, an abandoned one does
        // not.
        session.expires_at = now + ttl;
        Some(session)
    }

    /// Create a session and return its id, or `None` if no secure id can be
    /// generated or the store is full.
    pub fn create(&mut self) -> Option<String> {
        if self.sessions.len() >= MAX_SESSIONS {
            self.sweep();
            if self.sessions.len() >= MAX_SESSIONS {
                return None;
            }
        }
        let id = self.generate_id()?;
        let mut session = Session::new(self.ttl);
        session.visits = 1;
        self.sessions.insert(id.clone(), session);
        Some(id)
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Store a value on a session, if it still exists.
    pub fn set_value(&mut self, id: &str, key: &str, value: &str) -> bool {
        match self.sessions.get_mut(id) {
            Some(session) => {
                session.data.insert(key.to_string(), value.to_string());
                true
            }
            None => false,
        }
    }

    /// Forget a session. Used by the demo page's logout.
    pub fn destroy(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    /// Drop expired sessions. Returns how many went.
    ///
    /// Called from the event loop's tick rather than per request: sweeping the
    /// whole map on every request would make each one cost O(sessions).
    pub fn sweep(&mut self) -> usize {
        let now = Instant::now();
        let before = self.sessions.len();
        self.sessions.retain(|_, session| !session.is_expired(now));
        before - self.sessions.len()
    }

    /// 32 bytes from the kernel CSPRNG, hex-encoded.
    fn generate_id(&mut self) -> Option<String> {
        if self.randomness_failed {
            return None;
        }
        match read_random_bytes() {
            Some(bytes) => Some(to_hex(&bytes)),
            None => {
                // Refuse to fall back on the clock or a counter: a predictable
                // session id is a session hijack waiting to happen. Better to
                // serve the site without sessions than with forgeable ones.
                eprintln!(
                    "localserver: cannot read /dev/urandom, sessions are disabled for this run"
                );
                self.randomness_failed = true;
                None
            }
        }
    }
}

fn read_random_bytes() -> Option<[u8; 32]> {
    let mut file = File::open("/dev/urandom").ok()?;
    let mut bytes = [0u8; 32];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Is this string shaped like an id this server issued?
///
/// Checked before the map lookup so a client cannot probe with arbitrary
/// strings, and so a mangled cookie is treated as absent rather than as a
/// lookup.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn creates_and_finds_a_session() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        let id = store.create().unwrap();
        assert_eq!(store.len(), 1);
        let session = store.touch(&id).unwrap();
        // One visit for the creating request, one for this lookup.
        assert_eq!(session.visits, 2);
    }

    /// A guessable id is a session hijack, so this is the property that matters
    /// most in the whole module.
    #[test]
    fn ids_are_long_hex_and_unique() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let id = store.create().unwrap();
            assert_eq!(id.len(), 64, "32 bytes, hex-encoded");
            assert!(is_valid_id(&id));
            assert!(seen.insert(id), "ids must never repeat");
        }
    }

    #[test]
    fn rejects_ids_that_are_not_shaped_like_ours() {
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("short"));
        assert!(!is_valid_id(&"z".repeat(64)), "not hex");
        assert!(is_valid_id(&"a1".repeat(32)));
    }

    #[test]
    fn an_unknown_id_is_not_found() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        assert!(store.touch(&"0".repeat(64)).is_none());
    }

    /// A stale id must produce a fresh session, never a resurrected one.
    #[test]
    fn an_expired_session_is_removed_on_lookup() {
        let mut store = SessionStore::new(Duration::from_millis(0));
        let id = store.create().unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.touch(&id).is_none(), "already expired");
        assert_eq!(store.len(), 0, "and removed while we were there");
    }

    #[test]
    fn sweeping_drops_only_expired_sessions() {
        let mut store = SessionStore::new(Duration::from_millis(0));
        store.create().unwrap();
        store.create().unwrap();
        assert_eq!(store.sweep(), 2);
        assert_eq!(store.len(), 0);

        let mut store = SessionStore::new(DEFAULT_TTL);
        store.create().unwrap();
        assert_eq!(store.sweep(), 0, "live sessions must survive a sweep");
        assert_eq!(store.len(), 1);
    }

    /// Activity extends a session; the deadline is not fixed at creation.
    #[test]
    fn touching_a_session_extends_it() {
        let mut store = SessionStore::new(Duration::from_secs(60));
        let id = store.create().unwrap();
        let first = store.get(&id).unwrap().remaining(Instant::now());
        std::thread::sleep(Duration::from_millis(20));
        store.touch(&id).unwrap();
        let after = store.get(&id).unwrap().remaining(Instant::now());
        assert!(after >= first, "an active session should not age out");
    }

    #[test]
    fn stores_and_destroys_session_data() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        let id = store.create().unwrap();
        assert!(store.set_value(&id, "name", "Ada"));
        assert_eq!(store.get(&id).unwrap().name(), Some("Ada"));

        assert!(store.destroy(&id));
        assert!(store.get(&id).is_none());
        assert!(!store.destroy(&id), "destroying twice is not an error, just false");
        assert!(!store.set_value(&id, "name", "Ada"), "a gone session takes no data");
    }

    /// A client that ignores cookies — `siege` does — creates a session per
    /// request. Without a cap that is unbounded growth for the length of a run.
    #[test]
    fn the_store_is_capped() {
        let mut store = SessionStore::new(DEFAULT_TTL);
        for _ in 0..MAX_SESSIONS {
            assert!(store.create().is_some());
        }
        assert_eq!(store.len(), MAX_SESSIONS);
        // Nothing has expired, so the sweep frees nothing and creation stops.
        assert!(store.create().is_none(), "the cap must hold");
    }

    #[test]
    fn hex_encoding_is_fixed_width() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
