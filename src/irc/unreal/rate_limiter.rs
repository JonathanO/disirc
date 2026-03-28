// Called by the connection loop (implemented in the next task).
#![allow(dead_code)]

//! Token-bucket rate limiter for outbound IRC messages.
//!
//! Spec (spec-02):
//! - Bucket capacity: 10 tokens
//! - Refill rate: 1 token per 500 ms
//! - `PING` and `PONG` bypass the limiter entirely
//!
//! When the bucket is empty [`TokenBucket::try_consume`] returns `false` and
//! the caller must queue the message and retry when tokens are available.
//! [`TokenBucket::refill_delay`] returns the `Duration` until the next token
//! arrives, suitable for use with `tokio::time::sleep`.

use std::time::{Duration, Instant};

/// Spec constants.
pub const BUCKET_CAPACITY: u32 = 10;
/// Refill interval: one new token every 500 ms.
pub const REFILL_INTERVAL: Duration = Duration::from_millis(500);

/// A token-bucket rate limiter with fractional-token accumulation.
///
/// Tokens accumulate continuously (not in discrete 500 ms steps) so callers
/// that send a burst and then wait 250 ms get 0.5 tokens back, allowing
/// smoother throughput near the limit.
pub struct TokenBucket {
    /// Remaining tokens (0.0 – capacity).
    tokens: f64,
    /// Maximum token count.
    capacity: f64,
    /// Tokens added per millisecond.
    rate_per_ms: f64,
    /// Wall-clock time of last call to refill().
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a new bucket, starting full.
    pub fn new(capacity: u32, refill_interval: Duration) -> Self {
        let capacity = f64::from(capacity);
        Self {
            tokens: capacity,
            capacity,
            rate_per_ms: 1.0 / refill_interval.as_millis() as f64,
            last_refill: Instant::now(),
        }
    }

    /// Create a bucket with the spec defaults (capacity 10, 1/500 ms).
    pub fn default_irc() -> Self {
        Self::new(BUCKET_CAPACITY, REFILL_INTERVAL)
    }

    /// Refill tokens based on elapsed time since the last refill, then try to
    /// consume one token.
    ///
    /// Returns `true` if a token was available and consumed; `false` if the
    /// bucket is empty.
    pub fn try_consume(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Duration until the next token is available.
    ///
    /// If a token is available right now the returned duration is zero.
    /// Useful for scheduling a retry: `tokio::time::sleep(bucket.refill_delay(Instant::now()))`.
    pub fn refill_delay(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            let needed = 1.0 - self.tokens;
            Duration::from_millis((needed / self.rate_per_ms).ceil() as u64)
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed_ms = now.duration_since(self.last_refill).as_millis() as f64;
        self.tokens = (self.tokens + elapsed_ms * self.rate_per_ms).min(self.capacity);
        self.last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> TokenBucket {
        TokenBucket::new(BUCKET_CAPACITY, REFILL_INTERVAL)
    }

    // Helper: create a bucket with a specific token count (bypassing wall clock).
    fn bucket_with_tokens(tokens: f64) -> TokenBucket {
        let mut b = bucket();
        b.tokens = tokens;
        b.last_refill = Instant::now();
        b
    }

    #[test]
    fn starts_full_and_consumes_capacity_tokens() {
        let mut b = bucket();
        let now = Instant::now();
        for _ in 0..BUCKET_CAPACITY {
            assert!(b.try_consume(now), "expected token to be available");
        }
    }

    #[test]
    fn bucket_empty_after_capacity_consumed() {
        let mut b = bucket();
        let now = Instant::now();
        for _ in 0..BUCKET_CAPACITY {
            b.try_consume(now);
        }
        assert!(!b.try_consume(now), "bucket should be empty");
    }

    #[test]
    fn refills_one_token_after_one_interval() {
        let mut b = bucket_with_tokens(0.0);
        // Simulate exactly one refill interval having elapsed.
        let later = b.last_refill + REFILL_INTERVAL;
        assert!(
            b.try_consume(later),
            "should have one token after one interval"
        );
    }

    #[test]
    fn partial_refill_does_not_grant_token() {
        let mut b = bucket_with_tokens(0.0);
        // Only half an interval has elapsed → 0.5 tokens → still no token.
        let later = b.last_refill + REFILL_INTERVAL / 2;
        assert!(
            !b.try_consume(later),
            "half interval should not yield a token"
        );
    }

    #[test]
    fn does_not_exceed_capacity_on_refill() {
        let mut b = bucket(); // starts full (10 tokens)
        // Simulate 10 intervals passing — tokens must not exceed 10.
        let later = b.last_refill + REFILL_INTERVAL * 10;
        b.refill(later);
        assert!(
            b.tokens <= b.capacity,
            "tokens ({}) exceeded capacity ({})",
            b.tokens,
            b.capacity
        );
    }

    #[test]
    fn refill_delay_zero_when_token_available() {
        let mut b = bucket(); // full
        let delay = b.refill_delay(Instant::now());
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn refill_delay_positive_when_empty() {
        let mut b = bucket_with_tokens(0.0);
        let delay = b.refill_delay(b.last_refill);
        assert!(
            delay > Duration::ZERO,
            "expected positive delay when bucket is empty"
        );
        assert!(
            delay <= REFILL_INTERVAL,
            "delay ({delay:?}) should not exceed one refill interval"
        );
    }

    #[test]
    fn refill_delay_proportional_to_shortage() {
        // With 0.5 tokens short, we need ~250 ms.
        let mut b = bucket_with_tokens(0.5);
        let delay = b.refill_delay(b.last_refill);
        // ceil((0.5 / (1/500)) ms) = ceil(250) = 250 ms
        assert_eq!(delay, Duration::from_millis(250));
    }

    #[test]
    fn multiple_refills_accumulate_correctly() {
        let mut b = bucket_with_tokens(0.0);
        let t0 = b.last_refill;

        // Consume after 1 interval → 1 token available.
        assert!(b.try_consume(t0 + REFILL_INTERVAL));
        // Now bucket is back to 0.  After another interval: 1 token again.
        assert!(b.try_consume(t0 + REFILL_INTERVAL * 2));
        // Second consume at the same instant: no tokens.
        assert!(!b.try_consume(t0 + REFILL_INTERVAL * 2));
    }
}
