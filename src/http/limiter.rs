use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MILLIS_PER_MINUTE: f64 = 60_000.0;
const SECONDS_PER_MINUTE: u32 = 60;
const IDLE_TTL_MILLIS: u64 = 2 * 60_000;
const PURGE_INTERVAL_MILLIS: u64 = 60_000;

/// The source of time in milliseconds, injectable so the tests can move it.
pub type NowMillis = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_millis() -> NowMillis {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
    })
}

struct Bucket {
    tokens: f64,
    last_refill: u64,
}

struct State {
    buckets: HashMap<String, Bucket>,
    last_purge: u64,
}

/// One token bucket per key (user, client address): `per_minute` requests per minute, with a burst of the same size.
/// Idle keys are dropped so the map cannot grow forever.
pub struct KeyedLimiter {
    per_minute: u32,
    now: NowMillis,
    state: Mutex<State>,
}

opaque_debug!(KeyedLimiter);

impl KeyedLimiter {
    pub fn new(per_minute: u32, now: NowMillis) -> Self {
        let last_purge = now();
        Self { per_minute, now, state: Mutex::new(State { buckets: HashMap::new(), last_purge }) }
    }

    pub fn allow(&self, key: &str) -> bool {
        let now = (self.now)();
        let capacity = f64::from(self.per_minute);
        // A poisoned lock only means another request panicked mid-update; the counters are still usable.
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_sub(state.last_purge) > PURGE_INTERVAL_MILLIS {
            state.buckets.retain(|_, bucket| now.saturating_sub(bucket.last_refill) <= IDLE_TTL_MILLIS);
            state.last_purge = now;
        }
        let bucket = state.buckets.entry(key.to_owned()).or_insert(Bucket { tokens: capacity, last_refill: now });
        let elapsed = now.saturating_sub(bucket.last_refill) as f64;
        bucket.tokens = capacity.min(bucket.tokens + elapsed * capacity / MILLIS_PER_MINUTE);
        bucket.last_refill = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }

    /// How long until one more token is available, for the `Retry-After` header.
    pub fn retry_after_seconds(&self) -> String {
        (SECONDS_PER_MINUTE.div_ceil(self.per_minute.max(1))).to_string()
    }

    /// The number of keys being tracked.
    pub fn size(&self) -> usize {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn clock() -> (Arc<AtomicU64>, NowMillis) {
        let millis = Arc::new(AtomicU64::new(1_000_000));
        let reader = Arc::clone(&millis);
        (millis, Arc::new(move || reader.load(Ordering::SeqCst)))
    }

    #[test]
    fn it_allows_a_burst_of_the_size_of_the_rate_and_then_refuses() {
        let (_, now) = clock();
        let limiter = KeyedLimiter::new(3, now);
        assert!([(); 3].iter().all(|()| limiter.allow("a")));
        assert!(!limiter.allow("a"));
    }

    #[test]
    fn keys_do_not_share_a_bucket() {
        let (_, now) = clock();
        let limiter = KeyedLimiter::new(1, now);
        assert!(limiter.allow("a"));
        assert!(limiter.allow("b"));
        assert!(!limiter.allow("a"));
    }

    #[test]
    fn tokens_come_back_with_time() {
        let (millis, now) = clock();
        let limiter = KeyedLimiter::new(60, now);
        for _ in 0..60 {
            assert!(limiter.allow("a"));
        }
        assert!(!limiter.allow("a"));
        millis.fetch_add(1_000, Ordering::SeqCst);
        assert!(limiter.allow("a"), "one token per second at 60 per minute");
        assert!(!limiter.allow("a"));
    }

    #[test]
    fn the_bucket_never_holds_more_than_the_rate() {
        let (millis, now) = clock();
        let limiter = KeyedLimiter::new(2, now);
        assert!(limiter.allow("a"));
        millis.fetch_add(10 * 60_000, Ordering::SeqCst);
        assert!(limiter.allow("a") && limiter.allow("a"));
        assert!(!limiter.allow("a"));
    }

    #[test]
    fn idle_keys_are_dropped() {
        let (millis, now) = clock();
        let limiter = KeyedLimiter::new(5, now);
        limiter.allow("old");
        assert_eq!(limiter.size(), 1);
        millis.fetch_add(3 * 60_000, Ordering::SeqCst);
        limiter.allow("new");
        assert_eq!(limiter.size(), 1, "only the key seen after the purge is left");
    }

    #[test]
    fn retry_after_is_the_seconds_to_the_next_token() {
        let (_, now) = clock();
        assert_eq!(KeyedLimiter::new(120, Arc::clone(&now)).retry_after_seconds(), "1");
        assert_eq!(KeyedLimiter::new(30, Arc::clone(&now)).retry_after_seconds(), "2");
        assert_eq!(KeyedLimiter::new(7, now).retry_after_seconds(), "9");
    }
}
