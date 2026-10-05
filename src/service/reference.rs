/// Pure reference model for Token Bucket rate limiting.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBucketReference {
    pub capacity: f64,
    pub refill_per_ms: f64,
    pub window_ms: u64,
    pub tokens: f64,
    pub last_timestamp_ms: Option<u64>,
}

impl TokenBucketReference {
    pub fn new(capacity: f64, window_ms: u64) -> Self {
        let refill_per_ms = capacity / (window_ms.max(1) as f64);
        Self {
            capacity,
            refill_per_ms,
            window_ms,
            tokens: capacity,
            last_timestamp_ms: None,
        }
    }

    pub fn update_capacity(&mut self, new_capacity: f64) {
        self.capacity = new_capacity;
        self.refill_per_ms = new_capacity / (self.window_ms.max(1) as f64);
        if self.tokens > self.capacity {
            self.tokens = self.capacity;
        }
    }

    /// Advance time and consume `cost` tokens if available.
    /// Returns `(allowed, observed_tokens)` matching the Redis Lua script.
    pub fn consume(&mut self, now_ms: u64, cost: u64) -> (bool, i64) {
        let mut effective_now_ms = now_ms;
        if let Some(last) = self.last_timestamp_ms {
            if effective_now_ms < last {
                effective_now_ms = last;
            }
            let elapsed = effective_now_ms.saturating_sub(last);
            self.tokens = (self.tokens + (elapsed as f64 * self.refill_per_ms)).min(self.capacity);
        } else {
            self.tokens = self.capacity;
        }

        let allowed = self.tokens >= cost as f64;
        if allowed {
            self.tokens -= cost as f64;
        }
        self.last_timestamp_ms = Some(effective_now_ms);
        (allowed, self.tokens.floor() as i64)
    }

    /// Advance time and refund `refund_amount` tokens.
    /// Returns `(true, observed_tokens)` matching the Redis Lua script.
    pub fn refund(&mut self, now_ms: u64, refund_amount: u64) -> (bool, i64) {
        let mut effective_now_ms = now_ms;
        if let Some(last) = self.last_timestamp_ms {
            if effective_now_ms < last {
                effective_now_ms = last;
            }
            let elapsed = effective_now_ms.saturating_sub(last);
            self.tokens = (self.tokens + (elapsed as f64 * self.refill_per_ms)).min(self.capacity);
        } else {
            self.tokens = self.capacity;
        }

        self.tokens = (self.tokens + refund_amount as f64).min(self.capacity);
        self.last_timestamp_ms = Some(effective_now_ms);
        (true, self.tokens.floor() as i64)
    }
}

/// Pure reference model for Sliding Window rate limiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlidingWindowReference {
    pub window_ms: u64,
    pub limit: u64,
    pub max_retention: usize,
    /// Vector of (timestamp_ms, member_string) in insertion order
    pub events: Vec<(u64, String)>,
}

impl SlidingWindowReference {
    pub const DEFAULT_MAX_RETENTION: usize = 10_000;

    pub fn new(window_ms: u64, limit: u64) -> Self {
        Self {
            window_ms,
            limit,
            max_retention: Self::DEFAULT_MAX_RETENTION,
            events: Vec::new(),
        }
    }

    pub fn evict_expired(&mut self, now_ms: u64) {
        if now_ms > self.window_ms {
            let cutoff = now_ms - self.window_ms;
            self.events.retain(|(ts, _)| *ts > cutoff);
        }
    }

    /// Attempt to consume `hits` events.
    /// Returns `(allowed, current_count)` matching the Redis Lua script.
    pub fn consume(&mut self, now_ms: u64, now_usec: u64, nonce: &str, hits: u64) -> (bool, i64) {
        if !(1..=100).contains(&hits) {
            return (false, 0);
        }

        self.evict_expired(now_ms);
        let current = self.events.len() as u64;

        if current + hits <= self.limit {
            for i in 1..=hits {
                let member = format!("{now_usec}:{nonce}:{i}");
                self.events.push((now_ms, member));
            }
            let mut new_current = current + hits;
            if self.events.len() > self.max_retention {
                let excess = self.events.len() - self.max_retention;
                self.events.drain(0..excess);
                new_current = self.max_retention as u64;
            }
            (true, new_current as i64)
        } else {
            (false, current as i64)
        }
    }

    pub fn count(&self) -> usize {
        self.events.len()
    }
}
