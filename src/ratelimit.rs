//! A light per-source-IP token bucket, applied to posts and uploads.
//!
//! There is no auth to tell a buggy client stuck in a retry loop from normal
//! use, so without a limit one broken (not necessarily malicious) peer could
//! hammer the host's disk and CPU. This is a safety guard against accidents,
//! not an adversarial defense -- the trust-network model already assumes
//! admitted peers are not attackers -- so a simple in-memory bucket per mesh
//! IP is enough; nothing is persisted.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

/// Burst allowance: a peer may post this many times back-to-back before the
/// sustained rate applies.
const CAPACITY: f64 = 15.0;
/// Sustained refill, tokens per second (~one action every 2s once the burst
/// is spent). Comfortably above any human cadence, low enough to stop a hot
/// retry loop from filling the disk.
const REFILL_PER_SEC: f64 = 0.5;

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub struct RateLimiter {
    buckets: HashMap<IpAddr, Bucket>,
}

impl RateLimiter {
    /// Try to spend one token for `ip`. Returns true if allowed. Refills
    /// lazily from elapsed time on each check -- no background sweep needed.
    pub fn check(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let b = self.buckets.entry(ip).or_insert(Bucket { tokens: CAPACITY, last: now });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * REFILL_PER_SEC).min(CAPACITY);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
